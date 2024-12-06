//! This module handles [`CDFScanFile`]s for [`TableChangeScan`]. A [`CDFScanFile`] consists of all the
//! metadata required to generate a change data feed. [`CDFScanFile`] can be constructed using
//! [`CDFScanFileVisitor`]. The visitor reads from engine data with the schema [`cdf_scan_row_schema`].
//! You can convert engine data to this schema using the [`get_cdf_scan_row_expression`].
use itertools::Itertools;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use super::log_replay::TableChangesScanData;
use crate::actions::visitors::visit_deletion_vector_at;
use crate::engine_data::{GetData, TypedGetData};
use crate::expressions::{column_expr, Expression};
use crate::scan::state::DvInfo;
use crate::schema::{
    ColumnName, ColumnNamesAndTypes, DataType, MapType, SchemaRef, StructField, StructType,
};
use crate::utils::require;
use crate::{DeltaResult, EngineData, Error, RowVisitor};

#[allow(unused)]
pub(crate) struct UnresolvedCDFScanFile {
    pub scan_file: CDFScanFile,
    pub remove_dvs: Arc<HashMap<String, DvInfo>>,
}
#[allow(unused)]
#[derive(Debug)]
pub(crate) struct ResolvedCDFScanFile {
    pub scan_file: CDFScanFile,
    pub selection_vector: Option<Vec<bool>>,
}

// The type of action associated with a [`CDFScanFile`].
#[allow(unused)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CDFScanFileType {
    Add,
    Remove,
    Cdc,
}

/// Represents all the metadata needed to read a Change Data Feed.
#[allow(unused)]
#[derive(Debug, PartialEq, Clone)]
pub(crate) struct CDFScanFile {
    /// The type of action this file belongs to. This may be one of add, remove, or cdc.
    pub scan_type: CDFScanFileType,
    /// a `&str` which is the path to the file
    pub path: String,
    /// a [`DvInfo`] struct, which allows getting the selection vector for this file
    pub dv_info: DvInfo,
    /// a `HashMap<String, String>` which are partition values
    pub partition_values: HashMap<String, String>,
    /// the commit version that this action was performed in
    pub commit_version: i64,
    /// the timestamp of the commit that this action was performed in
    pub commit_timestamp: i64,
}

pub(crate) type CDFScanCallback<T> = fn(context: &mut T, scan_file: CDFScanFile);

/// Transforms an iterator of TableChangesScanData into an iterator  of
/// `UnresolvedCDFScanFile` by visiting the engine data.
#[allow(unused)]
pub(crate) fn scan_data_to_scan_file(
    scan_data: impl Iterator<Item = DeltaResult<TableChangesScanData>>,
) -> impl Iterator<Item = DeltaResult<UnresolvedCDFScanFile>> {
    scan_data
        .map(|scan_data| -> DeltaResult<_> {
            let scan_data = scan_data?;
            let callback: CDFScanCallback<Vec<CDFScanFile>> =
                |context, scan_file| context.push(scan_file);
            let result = visit_cdf_scan_files(
                scan_data.scan_data.as_ref(),
                &scan_data.selection_vector,
                vec![],
                callback,
            )?
            .into_iter()
            .map(move |scan_file| UnresolvedCDFScanFile {
                scan_file,
                remove_dvs: scan_data.remove_dvs.clone(),
            });
            Ok(result)
        }) // Iterator-Result-Iterator
        .flatten_ok() // Iterator-Result
}

/// Request that the kernel call a callback on each valid file that needs to be read for the
/// scan.
///
/// The arguments to the callback are:
/// * `context`: an `&mut context` argument. this can be anything that engine needs to pass through to each call
/// * `CDFScanFile`: a [`CDFScanFile`] struct that holds all the metadata required to perform Change Data
///   Feed
///
/// ## Context
/// A note on the `context`. This can be any value the engine wants. This function takes ownership
/// of the passed arg, but then returns it, so the engine can repeatedly call `visit_cdf_scan_files`
/// with the same context.
///
/// ## Example
/// ```ignore
/// let mut context = [my context];
/// for res in scan_data { // scan data table_changes_scan.scan_data()
///     let (data, vector, remove_dv) = res?;
///     context = delta_kernel::table_changes::scan_file::visit_cdf_scan_files(
///        data.as_ref(),
///        selection_vector,
///        context,
///        my_callback,
///     )?;
/// }
/// ```
#[allow(unused)]
pub(crate) fn visit_cdf_scan_files<T>(
    data: &dyn EngineData,
    selection_vector: &[bool],
    context: T,
    callback: CDFScanCallback<T>,
) -> DeltaResult<T> {
    let mut visitor = CDFScanFileVisitor {
        callback,
        selection_vector,
        context,
    };

    visitor.visit_rows_of(data)?;
    Ok(visitor.context)
}

// add some visitor magic for engines
#[allow(unused)]
struct CDFScanFileVisitor<'a, T> {
    callback: CDFScanCallback<T>,
    selection_vector: &'a [bool],
    context: T,
}

impl<T> RowVisitor for CDFScanFileVisitor<'_, T> {
    fn visit<'a>(&mut self, row_count: usize, getters: &[&'a dyn GetData<'a>]) -> DeltaResult<()> {
        require!(
            getters.len() == 18,
            Error::InternalError(format!(
                "Wrong number of CDFScanFileVisitor getters: {}",
                getters.len()
            ))
        );
        for row_index in 0..row_count {
            if !self.selection_vector[row_index] {
                continue;
            }

            let (scan_type, path, deletion_vector, partition_values) =
                if let Some(path) = getters[0].get_opt(row_index, "scanFile.add.path")? {
                    let scan_type = CDFScanFileType::Add;
                    let deletion_vector = visit_deletion_vector_at(row_index, &getters[1..=5])?;
                    let partition_values = getters[6]
                        .get(row_index, "scanFile.add.fileConstantValues.partitionValues")?;
                    (scan_type, path, deletion_vector, partition_values)
                } else if let Some(path) = getters[7].get_opt(row_index, "scanFile.remove.path")? {
                    let scan_type = CDFScanFileType::Remove;
                    let deletion_vector = visit_deletion_vector_at(row_index, &getters[8..=12])?;
                    let partition_values = getters[13].get(
                        row_index,
                        "scanFile.remove.fileConstantValues.partitionValues",
                    )?;
                    (scan_type, path, deletion_vector, partition_values)
                } else if let Some(path) = getters[14].get_opt(row_index, "scanFile.cdc.path")? {
                    let scan_type = CDFScanFileType::Cdc;
                    let partition_values = getters[15]
                        .get(row_index, "scanFile.cdc.fileConstantValues.partitionValues")?;
                    (scan_type, path, None, partition_values)
                } else {
                    continue;
                };
            let dv_info = DvInfo { deletion_vector };
            let scan_file = CDFScanFile {
                scan_type,
                path,
                dv_info,
                partition_values,
                commit_timestamp: getters[16].get(row_index, "scanFile.timestamp")?,
                commit_version: getters[17].get(row_index, "scanFile.commit_version")?,
            };
            (self.callback)(&mut self.context, scan_file)
        }
        Ok(())
    }

    fn selected_column_names_and_types(&self) -> (&'static [ColumnName], &'static [DataType]) {
        static NAMES_AND_TYPES: LazyLock<ColumnNamesAndTypes> =
            LazyLock::new(|| cdf_scan_row_schema().leaves(None));
        NAMES_AND_TYPES.as_ref()
    }
}

/// Get the schema that scan rows (from [`TableChanges::scan_data`]) will be returned with.
pub(crate) fn cdf_scan_row_schema() -> SchemaRef {
    static CDF_SCAN_ROW_SCHEMA: LazyLock<Arc<StructType>> = LazyLock::new(|| {
        let deletion_vector = StructType::new([
            StructField::new("storageType", DataType::STRING, true),
            StructField::new("pathOrInlineDv", DataType::STRING, true),
            StructField::new("offset", DataType::INTEGER, true),
            StructField::new("sizeInBytes", DataType::INTEGER, true),
            StructField::new("cardinality", DataType::LONG, true),
        ]);
        let partition_values = MapType::new(DataType::STRING, DataType::STRING, true);
        let file_constant_values =
            StructType::new([StructField::new("partitionValues", partition_values, true)]);

        let add = StructType::new([
            StructField::new("path", DataType::STRING, true),
            StructField::new("deletionVector", deletion_vector.clone(), true),
            StructField::new("fileConstantValues", file_constant_values.clone(), true),
        ]);
        let remove = StructType::new([
            StructField::new("path", DataType::STRING, true),
            StructField::new("deletionVector", deletion_vector, true),
            StructField::new("fileConstantValues", file_constant_values.clone(), true),
        ]);
        let cdc = StructType::new([
            StructField::new("path", DataType::STRING, true),
            StructField::new("fileConstantValues", file_constant_values, true),
        ]);

        Arc::new(StructType::new([
            StructField::new("add", add, true),
            StructField::new("remove", remove, true),
            StructField::new("cdc", cdc, true),
            StructField::new("timestamp", DataType::LONG, true),
            StructField::new("commit_version", DataType::LONG, true),
        ]))
    });
    CDF_SCAN_ROW_SCHEMA.clone()
}

/// Expression to convert an action with `log_schema` into one with
/// `TABLE_CHANGES_cdf_scan_row_schema`. This is the expression used to create `TableChangesScanData`.
#[allow(unused)]
pub(crate) fn get_cdf_scan_row_expression(commit_timestamp: i64, commit_number: i64) -> Expression {
    Expression::struct_from([
        Expression::struct_from([
            column_expr!("add.path"),
            column_expr!("add.deletionVector"),
            Expression::struct_from([column_expr!("add.partitionValues")]),
        ]),
        Expression::struct_from([
            column_expr!("remove.path"),
            column_expr!("remove.deletionVector"),
            Expression::struct_from([column_expr!("remove.partitionValues")]),
        ]),
        Expression::struct_from([
            column_expr!("cdc.path"),
            Expression::struct_from([column_expr!("cdc.partitionValues")]),
        ]),
        commit_timestamp.into(),
        commit_number.into(),
    ])
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use itertools::Itertools;

    use super::CDFScanFileType;
    use super::{
        cdf_scan_row_schema, get_cdf_scan_row_expression, visit_cdf_scan_files, CDFScanCallback,
        CDFScanFile,
    };
    use crate::actions::deletion_vector::DeletionVectorDescriptor;
    use crate::actions::{get_log_schema, Add, Cdc, Remove};
    use crate::engine::sync::SyncEngine;
    use crate::log_segment::LogSegment;
    use crate::scan::state::DvInfo;
    use crate::utils::test_utils::{Action, LocalMockTable};
    use crate::{DeltaResult, Engine};

    #[tokio::test]
    async fn schema_transform_correct() {
        let engine = SyncEngine::new();
        let mut mock_table = LocalMockTable::new();

        let add_dv = DeletionVectorDescriptor {
            storage_type: "u".to_string(),
            path_or_inline_dv: "vBn[lx{q8@P<9BNH/isA".to_string(),
            offset: Some(1),
            size_in_bytes: 36,
            cardinality: 2,
        };
        let add_partition_values = HashMap::from([("a".to_string(), "b".to_string())]);
        let add = Add {
            path: "fake_path_1".into(),
            deletion_vector: Some(add_dv.clone()),
            partition_values: add_partition_values,
            ..Default::default()
        };

        let rm_dv = DeletionVectorDescriptor {
            storage_type: "u".to_string(),
            path_or_inline_dv: "U5OWRz5k%CFT.Td}yCPW".to_string(),
            offset: Some(1),
            size_in_bytes: 38,
            cardinality: 3,
        };
        let rm_partition_values = Some(HashMap::from([("c".to_string(), "d".to_string())]));
        let remove = Remove {
            path: "fake_path_2".into(),
            deletion_vector: Some(rm_dv),
            partition_values: rm_partition_values,
            ..Default::default()
        };

        let cdc_partition_values = HashMap::from([("x".to_string(), "y".to_string())]);
        let cdc = Cdc {
            path: "fake_path_3".into(),
            partition_values: cdc_partition_values,
            ..Default::default()
        };

        mock_table
            .commit([
                Action::Add(add.clone()),
                Action::Remove(remove.clone()),
                Action::Cdc(cdc.clone()),
            ])
            .await;

        let table_root = url::Url::from_directory_path(mock_table.table_root()).unwrap();
        let log_root = table_root.join("_delta_log/").unwrap();
        let log_segment =
            LogSegment::for_table_changes(engine.get_file_system_client().as_ref(), log_root, 0, 0)
                .unwrap();
        let commit = log_segment.ascending_commit_files[0].clone();

        let actions = engine
            .get_json_handler()
            .read_json_files(&[commit.location.clone()], get_log_schema().clone(), None)
            .unwrap();

        // Transform the engine data into the [`cdf_scan_row_schema`] and insert
        // the following timestamp and commit version.
        let commit_timestamp = 1234_i64;
        let commit_version = 42_i64;
        let scan_files: Vec<_> = actions
            .map_ok(|actions| {
                engine
                    .get_expression_handler()
                    .get_evaluator(
                        get_log_schema().clone(),
                        get_cdf_scan_row_expression(commit_timestamp, commit_version),
                        cdf_scan_row_schema().into(),
                    )
                    .evaluate(actions.as_ref())
                    .unwrap()
            })
            .map(|data| -> DeltaResult<_> {
                let data = data?;
                let selection_vector = vec![true; data.len()];
                let callback: CDFScanCallback<Vec<CDFScanFile>> =
                    |context, scan_file| context.push(scan_file);
                visit_cdf_scan_files(data.as_ref(), &selection_vector, vec![], callback)
            })
            .flatten_ok()
            .try_collect()
            .unwrap();

        let expected_scan_files = vec![
            CDFScanFile {
                scan_type: CDFScanFileType::Add,
                path: add.path,
                dv_info: DvInfo {
                    deletion_vector: add.deletion_vector,
                },
                partition_values: add.partition_values,
                commit_version,
                commit_timestamp,
            },
            CDFScanFile {
                scan_type: CDFScanFileType::Remove,
                path: remove.path,
                dv_info: DvInfo {
                    deletion_vector: remove.deletion_vector,
                },
                partition_values: remove.partition_values.unwrap(),
                commit_version,
                commit_timestamp,
            },
            CDFScanFile {
                scan_type: CDFScanFileType::Cdc,
                path: cdc.path,
                dv_info: DvInfo {
                    deletion_vector: None,
                },
                partition_values: cdc.partition_values,
                commit_version,
                commit_timestamp,
            },
        ];
        assert_eq!(expected_scan_files, scan_files);
    }
}
