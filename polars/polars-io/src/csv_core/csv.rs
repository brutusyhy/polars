use crate::csv::CsvEncoding;
use crate::csv_core::utils::*;
use crate::csv_core::{buffer::*, parser::*};
use crate::PhysicalIoExpr;
use crate::ScanAggregation;
use csv::ByteRecordsIntoIter;
use polars_arrow::array::*;
use polars_core::utils::accumulate_dataframes_vertical;
use polars_core::{prelude::*, POOL};
use rayon::prelude::*;
use rayon::ThreadPoolBuilder;
use std::fmt;
use std::io::{Read, Seek};
use std::sync::atomic::Ordering;
use std::sync::{atomic::AtomicUsize, Arc};

/// CSV file reader
pub struct SequentialReader<R: Read> {
    /// Explicit schema for the CSV file
    schema: SchemaRef,
    /// Optional projection for which columns to load (zero-based column indices)
    projection: Option<Vec<usize>>,
    /// File reader
    record_iter: Option<ByteRecordsIntoIter<R>>,
    /// Batch size (number of records to load each time)
    batch_size: usize,
    /// Current line number, used in error reporting
    line_number: usize,
    ignore_parser_errors: bool,
    skip_rows: usize,
    n_rows: Option<usize>,
    encoding: CsvEncoding,
    n_threads: Option<usize>,
    path: Option<String>,
    has_header: bool,
    delimiter: u8,
    sample_size: usize,
    chunk_size: usize,
}

impl<R> fmt::Debug for SequentialReader<R>
where
    R: Read,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reader")
            .field("schema", &self.schema)
            .field("projection", &self.projection)
            .field("batch_size", &self.batch_size)
            .field("line_number", &self.line_number)
            .finish()
    }
}

impl<R: Read + Sync + Send> SequentialReader<R> {
    /// Returns the schema of the reader, useful for getting the schema without reading
    /// record batches
    pub fn schema(&self) -> SchemaRef {
        match &self.projection {
            Some(projection) => {
                let fields = self.schema.fields();
                let projected_fields: Vec<Field> =
                    projection.iter().map(|i| fields[*i].clone()).collect();

                Arc::new(Schema::new(projected_fields))
            }
            None => self.schema.clone(),
        }
    }

    /// Create a new CsvReader from a `BufReader<R: Read>
    ///
    /// This constructor allows you more flexibility in what records are processed by the
    /// csv reader.
    #[allow(clippy::too_many_arguments)]
    pub fn from_reader(
        reader: R,
        schema: SchemaRef,
        has_header: bool,
        delimiter: u8,
        batch_size: usize,
        projection: Option<Vec<usize>>,
        ignore_parser_errors: bool,
        n_rows: Option<usize>,
        skip_rows: usize,
        encoding: CsvEncoding,
        n_threads: Option<usize>,
        path: Option<String>,
        sample_size: usize,
        chunk_size: usize,
    ) -> Self {
        let csv_reader = init_csv_reader(reader, has_header, delimiter);
        let record_iter = Some(csv_reader.into_byte_records());

        Self {
            schema,
            projection,
            record_iter,
            batch_size,
            line_number: if has_header { 1 } else { 0 },
            ignore_parser_errors,
            skip_rows,
            n_rows,
            encoding,
            n_threads,
            path,
            has_header,
            delimiter,
            sample_size,
            chunk_size,
        }
    }

    fn find_starting_point<'a>(&self, mut bytes: &'a [u8]) -> Result<&'a [u8]> {
        // Skip all leading white space and the occasional utf8-bom
        bytes = skip_line_ending(skip_whitespace(skip_bom(bytes)).0).0;

        // If there is a header we skip it.
        if self.has_header {
            bytes = skip_header(bytes).0;
        }

        if self.skip_rows > 0 {
            for _ in 0..self.skip_rows {
                let pos = next_line_position(bytes, self.schema.fields().len(), self.delimiter)
                    .ok_or_else(|| PolarsError::NoData("not enough lines to skip".into()))?;
                bytes = &bytes[pos..];
            }
        }
        Ok(bytes)
    }

    fn parse_csv(
        &mut self,
        n_threads: usize,
        bytes: &[u8],
        predicate: Option<&Arc<dyn PhysicalIoExpr>>,
    ) -> Result<DataFrame> {
        let logging = std::env::var("POLARS_VERBOSE").is_ok();

        // Make the variable mutable so that we can reassign the sliced file to this variable.
        let mut bytes = self.find_starting_point(bytes)?;

        // initial row guess. We use the line statistic to guess the number of rows to allocate
        let mut total_rows = 128;

        // if None, there are less then 128 rows in the file and the statistics don't matter that much
        if let Some((mean, std)) = get_line_stats(bytes, self.sample_size) {
            if logging {
                eprintln!("avg line length: {}\nstd. dev. line length: {}", mean, std);
            }

            // x % upper bound of byte length per line assuming normally distributed
            let line_length_upper_bound = mean + 1.1 * std;
            total_rows = (bytes.len() as f32 / (mean - 0.01 * std)) as usize;

            // if we only need to parse n_rows,
            // we first try to use the line statistics the total bytes we need to process
            if let Some(n_rows) = self.n_rows {
                total_rows = std::cmp::min(n_rows, total_rows);

                // the guessed upper bound of  the no. of bytes in the file
                let n_bytes = (line_length_upper_bound * (n_rows as f32)) as usize;

                if n_bytes < bytes.len() {
                    if let Some(pos) = next_line_position(
                        &bytes[n_bytes..],
                        self.schema.fields().len(),
                        self.delimiter,
                    ) {
                        bytes = &bytes[..n_bytes + pos]
                    }
                }
            }
            if logging {
                eprintln!("initial row estimate: {}", total_rows)
            }
        }
        if logging {
            eprintln!("file < 128 rows, no statistics determined")
        }

        // we also need to sort the projection to have predictable output.
        // the `parse_lines` function expects this.
        let projection = self
            .projection
            .take()
            .map(|mut v| {
                v.sort_unstable();
                v
            })
            .unwrap_or_else(|| (0..self.schema.fields().len()).collect());

        let chunk_size = std::cmp::min(self.chunk_size, total_rows);
        let n_chunks = total_rows / chunk_size;
        if logging {
            eprintln!(
                "no. of chunks: {} processed by: {} threads at 1 chunk/thread",
                n_chunks, n_threads
            );
        }

        // keep track of the maximum capacity that needs to be allocated for the utf8-builder
        // Per string column we keep a statistic of the maximum length of string bytes per chunk
        let str_columns: Vec<_> = projection
            .iter()
            .copied()
            .filter(|i| self.schema.field(*i).unwrap().data_type() == &DataType::Utf8)
            .collect();
        let init_str_bytes = chunk_size * 100;
        let str_capacities: Vec<_> = str_columns
            .iter()
            .map(|_| AtomicUsize::new(init_str_bytes))
            .collect();

        // split the file by the nearest new line characters such that every thread processes
        // approximately the same number of rows.
        let file_chunks =
            get_file_chunks(bytes, n_threads, self.schema.fields().len(), self.delimiter);
        let local_capacity = total_rows / n_threads;

        // If the number of threads given by the user is lower than our global thread pool we create
        // new one.
        let owned_pool;
        let pool = if POOL.current_num_threads() != n_threads {
            owned_pool = Some(
                ThreadPoolBuilder::new()
                    .num_threads(n_threads)
                    .build()
                    .unwrap(),
            );
            owned_pool.as_ref().unwrap()
        } else {
            &POOL
        };

        // all the buffers returned from the threads
        // Structure:
        //      the inner vec has got buffers from all the columns.
        let dfs = pool.install(|| {
            file_chunks
                .into_par_iter()
                .map(|(bytes_offset_thread, stop_at_nbytes)| {
                    let delimiter = self.delimiter;
                    let schema = self.schema.clone();
                    let ignore_parser_errors = self.ignore_parser_errors;
                    let projection = &projection;

                    let mut buffers = init_buffers(
                        &projection,
                        local_capacity,
                        &schema,
                        &str_capacities,
                        self.delimiter,
                    )?;
                    let local_bytes = &bytes[bytes_offset_thread..stop_at_nbytes];
                    let read = bytes_offset_thread;

                    parse_lines(
                        local_bytes,
                        read,
                        delimiter,
                        projection,
                        &mut buffers,
                        ignore_parser_errors,
                        self.encoding,
                    )?;
                    let mut df = DataFrame::new_no_checks(
                        buffers.into_iter().map(|buf| buf.into_series()).collect(),
                    );
                    if let Some(predicate) = predicate {
                        let s = predicate.evaluate(&df)?;
                        let mask = s.bool().expect("filter predicates was not of type boolean");
                        df = df.filter(mask)?;
                    }

                    let mut str_index = 0;
                    // update the running str bytes statistics
                    str_columns.iter().for_each(|&i| {
                        let ca = df.select_at_idx(i).unwrap().utf8().unwrap();
                        let str_bytes_len = ca.get_values_size();
                        // TODO! determine Ordering
                        let prev_value =
                            str_capacities[str_index].fetch_max(str_bytes_len, Ordering::SeqCst);
                        let prev_cap = (prev_value as f32 * 1.2) as usize;
                        if logging && (prev_cap < str_bytes_len) {
                            eprintln!(
                                "needed to reallocate column: {}\
                            \nprevious capacity was: {}\
                            \nneeded capacity was: {}",
                                self.schema.field(i).unwrap().name(),
                                prev_cap,
                                str_bytes_len
                            );
                        }
                        str_index += 1;
                    });

                    Ok(df)
                })
                .collect::<Result<Vec<_>>>()
        })?;

        accumulate_dataframes_vertical(dfs)
    }

    /// Read the csv into a DataFrame. The predicate can come from a lazy physical plan.
    pub fn as_df(
        &mut self,
        predicate: Option<Arc<dyn PhysicalIoExpr>>,
        aggregate: Option<&[ScanAggregation]>,
    ) -> Result<DataFrame> {
        let n_threads = self.n_threads.unwrap_or_else(num_cpus::get);

        let mut df = match (&self.path, self.record_iter.is_some()) {
            (Some(p), _) => {
                let file = std::fs::File::open(p).unwrap();
                let mmap = unsafe { memmap::Mmap::map(&file).unwrap() };
                let bytes = mmap[..].as_ref();
                self.parse_csv(n_threads, bytes, predicate.as_ref())?
            }
            (None, true) => {
                let mut r = std::mem::take(&mut self.record_iter).unwrap().into_reader();
                let mut bytes = Vec::with_capacity(1024 * 128);
                r.get_mut().read_to_end(&mut bytes)?;
                if !bytes.is_empty()
                    && (bytes[bytes.len() - 1] != b'\n' || bytes[bytes.len() - 1] != b'\r')
                {
                    bytes.push(b'\n')
                }
                self.parse_csv(n_threads, &bytes, predicate.as_ref())?
            }
            _ => return Err(PolarsError::Other("file or reader must be set".into())),
        };

        if let Some(aggregate) = aggregate {
            let cols = aggregate
                .iter()
                .map(|scan_agg| scan_agg.finish(&df).unwrap())
                .collect();
            df = DataFrame::new_no_checks(cols)
        }

        // if multi-threaded the n_rows was probabilistically determined.
        // Let's slice to correct number of rows if possible.
        if let Some(n_rows) = self.n_rows {
            if n_rows < df.height() {
                df = df.slice(0, n_rows).unwrap()
            }
        }
        Ok(df)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_csv_reader<R: 'static + Read + Seek + Sync + Send>(
    mut reader: R,
    n_rows: Option<usize>,
    skip_rows: usize,
    mut projection: Option<Vec<usize>>,
    batch_size: usize,
    max_records: Option<usize>,
    delimiter: Option<u8>,
    has_header: bool,
    ignore_parser_errors: bool,
    schema: Option<SchemaRef>,
    columns: Option<Vec<String>>,
    encoding: CsvEncoding,
    n_threads: Option<usize>,
    path: Option<String>,
    schema_overwrite: Option<&Schema>,
    sample_size: usize,
    chunk_size: usize,
) -> Result<SequentialReader<R>> {
    // check if schema should be inferred
    let delimiter = delimiter.unwrap_or(b',');
    let schema = match schema {
        Some(schema) => schema,
        None => {
            let (inferred_schema, _) = infer_file_schema(
                &mut reader,
                delimiter,
                max_records,
                has_header,
                schema_overwrite,
            )?;
            Arc::new(inferred_schema)
        }
    };

    if let Some(cols) = columns {
        let mut prj = Vec::with_capacity(cols.len());
        for col in cols {
            let i = schema.index_of(&col)?;
            prj.push(i);
        }
        projection = Some(prj);
    }

    Ok(SequentialReader::from_reader(
        reader,
        schema,
        has_header,
        delimiter,
        batch_size,
        projection,
        ignore_parser_errors,
        n_rows,
        skip_rows,
        encoding,
        n_threads,
        path,
        sample_size,
        chunk_size,
    ))
}
