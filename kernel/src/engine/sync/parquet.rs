use std::fs::File;

use arrow_schema::ArrowError;
use parquet::arrow::arrow_reader::{
    ArrowPredicateFn, ArrowReaderMetadata, ParquetRecordBatchReaderBuilder, RowFilter,
};
use parquet::arrow::ProjectionMask;
use tracing::debug;
use url::Url;

use crate::engine::arrow_data::ArrowEngineData;
use crate::engine::arrow_expression::{downcast_to_bool, evaluate_expression};
use crate::engine::arrow_utils::{generate_mask, get_requested_indices, reorder_struct_array};
use crate::schema::SchemaRef;
use crate::{DeltaResult, Error, Expression, FileDataReadResultIterator, FileMeta, ParquetHandler};

pub(crate) struct SyncParquetHandler;

fn try_create_from_parquet(
    schema: SchemaRef,
    location: Url,
    predicate: Option<Expression>,
) -> DeltaResult<ArrowEngineData> {
    let file = File::open(
        location
            .to_file_path()
            .map_err(|_| Error::generic("can only read local files"))?,
    )?;
    let metadata = ArrowReaderMetadata::load(&file, Default::default())?;
    let parquet_schema = metadata.schema();
    let mut builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let (indicies, requested_ordering) = get_requested_indices(&schema, parquet_schema)?;
    if let Some(mask) = generate_mask(&schema, parquet_schema, builder.parquet_schema(), &indicies)
    {
        builder = builder.with_projection(mask);
    }
    if let Some(predicate) = predicate {
        builder = builder.with_row_filter(RowFilter::new(vec![Box::new(ArrowPredicateFn::new(
            ProjectionMask::all(),
            move |batch| {
                downcast_to_bool(
                    &evaluate_expression(&predicate, &batch, None)
                        .map_err(|err| ArrowError::ExternalError(Box::new(err)))?,
                )
                .map_err(|err| ArrowError::ExternalError(Box::new(err)))
                .cloned()
            },
        ))]));
    }
    let mut reader = builder.build()?;
    let data = reader
        .next()
        .ok_or_else(|| Error::generic("No data found reading parquet file"))?;
    let reordered = reorder_struct_array(data?.into(), &requested_ordering).map(Into::into)?;
    Ok(ArrowEngineData::new(reordered))
}

impl ParquetHandler for SyncParquetHandler {
    fn read_parquet_files(
        &self,
        files: &[FileMeta],
        schema: SchemaRef,
        predicate: Option<Expression>,
    ) -> DeltaResult<FileDataReadResultIterator> {
        debug!("Reading parquet files: {files:#?} with schema {schema:#?} and predicate {predicate:#?}");
        if files.is_empty() {
            return Ok(Box::new(std::iter::empty()));
        }
        let locations: Vec<_> = files.iter().map(|file| file.location.clone()).collect();
        Ok(Box::new(locations.into_iter().map(move |location| {
            try_create_from_parquet(schema.clone(), location, predicate.clone())
                .map(|d| Box::new(d) as _)
        })))
    }
}
