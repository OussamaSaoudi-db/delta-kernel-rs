use super::TableChangesScanData;
use crate::actions::deletion_vector::DeletionVectorDescriptor;
use crate::actions::{Add, Cdc, Metadata, Protocol, Remove};
use crate::engine::sync::SyncEngine;
use crate::log_segment::LogSegment;
use crate::path::ParsedLogPath;
use crate::scan::state::DvInfo;
use crate::schema::{DataType, StructField, StructType};
use crate::table_changes::log_replay::LogReplayScanner;
use crate::utils::test_utils::LocalMockTable;
use crate::{DeltaResult, Engine, Error, Version};

use itertools::Itertools;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

fn get_schema() -> StructType {
    StructType::new([
        StructField::new("id", DataType::LONG, true),
        StructField::new("value", DataType::STRING, true),
    ])
}

fn get_segment(
    engine: &dyn Engine,
    path: &Path,
    start_version: Version,
    end_version: impl Into<Option<Version>>,
) -> DeltaResult<Vec<ParsedLogPath>> {
    let table_root = url::Url::from_directory_path(path).unwrap();
    let log_root = table_root.join("_delta_log/")?;
    let log_segment = LogSegment::for_table_changes(
        engine.get_file_system_client().as_ref(),
        log_root,
        start_version,
        end_version,
    )?;
    Ok(log_segment.ascending_commit_files)
}

fn result_to_sv(iter: impl Iterator<Item = DeltaResult<TableChangesScanData>>) -> Vec<bool> {
    iter.map_ok(|scan_data| scan_data.selection_vector.into_iter())
        .flatten_ok()
        .try_collect()
        .unwrap()
}

#[tokio::test]
async fn metadata_protocol() {
    let engine = Arc::new(SyncEngine::new());
    let mut mock_table = LocalMockTable::new();
    let schema_string = serde_json::to_string(&get_schema()).unwrap();
    mock_table
        .commit([
            Metadata {
                schema_string,
                configuration: HashMap::from([
                    ("delta.enableChangeDataFeed".to_string(), "true".to_string()),
                    (
                        "delta.enableDeletionVectors".to_string(),
                        "true".to_string(),
                    ),
                    ("dela.columnMapping.mode".to_string(), "none".to_string()),
                ]),
                ..Default::default()
            }
            .into(),
            Protocol::try_new(3, 7, Some(["deletionVectors"]), Some(["deletionVectors"]))
                .unwrap()
                .into(),
        ])
        .await;

    let mut commits = get_segment(engine.as_ref(), mock_table.table_root(), 0, None)
        .unwrap()
        .into_iter();

    let scanner = LogReplayScanner::prepare_table_changes(
        commits.next().unwrap(),
        engine.as_ref(),
        &get_schema().into(),
    )
    .unwrap();
    assert!(!scanner.has_cdc_action);
    assert!(scanner.remove_dvs.is_empty());

    assert_eq!(
        result_to_sv(scanner.into_scan_batches(engine.clone(), None).unwrap()),
        &[false, false]
    );
}
#[tokio::test]
async fn cdf_not_enabled() {
    let engine = Arc::new(SyncEngine::new());
    let mut mock_table = LocalMockTable::new();
    let schema_string = serde_json::to_string(&get_schema()).unwrap();
    mock_table
        .commit([Metadata {
            schema_string,
            configuration: HashMap::from([(
                "delta.enableDeletionVectors".to_string(),
                "true".to_string(),
            )]),
            ..Default::default()
        }
        .into()])
        .await;

    let mut commits = get_segment(engine.as_ref(), mock_table.table_root(), 0, None)
        .unwrap()
        .into_iter();

    let res = LogReplayScanner::prepare_table_changes(
        commits.next().unwrap(),
        engine.as_ref(),
        &get_schema().into(),
    );

    assert!(matches!(res, Err(Error::ChangeDataFeedUnsupported(_))));
}

#[tokio::test]
async fn unsupported_reader_feature() {
    let engine = Arc::new(SyncEngine::new());
    let mut mock_table = LocalMockTable::new();
    mock_table
        .commit([Protocol::try_new(
            3,
            7,
            Some(["deletionVectors", "columnMapping"]),
            Some([""; 0]),
        )
        .unwrap()
        .into()])
        .await;

    let mut commits = get_segment(engine.as_ref(), mock_table.table_root(), 0, None)
        .unwrap()
        .into_iter();

    let res = LogReplayScanner::prepare_table_changes(
        commits.next().unwrap(),
        engine.as_ref(),
        &get_schema().into(),
    );
    println!("Res: {:?}", res.err());

    //assert!(matches!(res, Err(Error::ChangeDataFeedUnsupported(_))));
}
#[tokio::test]
async fn column_mapping_should_fail() {
    let engine = Arc::new(SyncEngine::new());
    let mut mock_table = LocalMockTable::new();
    let schema_string = serde_json::to_string(&get_schema()).unwrap();
    mock_table
        .commit([Metadata {
            schema_string,
            configuration: HashMap::from([
                (
                    "delta.enableDeletionVectors".to_string(),
                    "true".to_string(),
                ),
                ("delta.enableChangeDataFeed".to_string(), "true".to_string()),
                ("delta.columnMapping.mode".to_string(), "id".to_string()),
            ]),
            ..Default::default()
        }
        .into()])
        .await;

    let mut commits = get_segment(engine.as_ref(), mock_table.table_root(), 0, None)
        .unwrap()
        .into_iter();

    let res = LogReplayScanner::prepare_table_changes(
        commits.next().unwrap(),
        engine.as_ref(),
        &get_schema().into(),
    );

    assert!(matches!(res, Err(Error::ChangeDataFeedUnsupported(_))));
}

#[tokio::test]
async fn incompatible_schema() {
    let engine = Arc::new(SyncEngine::new());
    let mut mock_table = LocalMockTable::new();

    // The original schema has two fields: `id` and value.
    let schema = get_schema().project(&["id"]).unwrap();
    let schema_string = serde_json::to_string(&schema).unwrap();
    mock_table
        .commit([Metadata {
            schema_string,
            configuration: HashMap::from([
                ("delta.enableChangeDataFeed".to_string(), "true".to_string()),
                (
                    "delta.enableDeletionVectors".to_string(),
                    "true".to_string(),
                ),
            ]),
            ..Default::default()
        }
        .into()])
        .await;

    let mut commits = get_segment(engine.as_ref(), mock_table.table_root(), 0, None)
        .unwrap()
        .into_iter();

    let res = LogReplayScanner::prepare_table_changes(
        commits.next().unwrap(),
        engine.as_ref(),
        &get_schema().into(),
    );

    assert!(matches!(
        res,
        Err(Error::ChangeDataFeedIncompatibleSchema(_, _))
    ));
}

#[tokio::test]
async fn add_remove() {
    let engine = Arc::new(SyncEngine::new());
    let mut mock_table = LocalMockTable::new();
    mock_table
        .commit([
            Add {
                path: "fake_path_1".into(),
                data_change: true,
                ..Default::default()
            }
            .into(),
            Remove {
                path: "fake_path_2".into(),
                data_change: true,
                ..Default::default()
            }
            .into(),
        ])
        .await;

    let mut commits = get_segment(engine.as_ref(), mock_table.table_root(), 0, None)
        .unwrap()
        .into_iter();

    let scanner = LogReplayScanner::prepare_table_changes(
        commits.next().unwrap(),
        engine.as_ref(),
        &get_schema().into(),
    )
    .unwrap();
    assert!(!scanner.has_cdc_action);
    assert_eq!(scanner.remove_dvs, HashMap::new());

    assert_eq!(
        result_to_sv(scanner.into_scan_batches(engine, None).unwrap()),
        &[true, true]
    );
}

#[tokio::test]
async fn filter_data_change() {
    let engine = Arc::new(SyncEngine::new());
    let mut mock_table = LocalMockTable::new();
    mock_table
        .commit([
            Remove {
                path: "fake_path_1".into(),
                data_change: false,
                ..Default::default()
            }
            .into(),
            Remove {
                path: "fake_path_2".into(),
                data_change: false,
                ..Default::default()
            }
            .into(),
            Remove {
                path: "fake_path_3".into(),
                data_change: false,
                ..Default::default()
            }
            .into(),
            Remove {
                path: "fake_path_4".into(),
                data_change: false,
                ..Default::default()
            }
            .into(),
            // Add action that has same path as remove
            Add {
                path: "fake_path_1".into(),
                data_change: false,
                ..Default::default()
            }
            .into(),
            // Add action with unique path
            Add {
                path: "fake_path_5".into(),
                data_change: false,
                ..Default::default()
            }
            .into(),
        ])
        .await;

    let mut commits = get_segment(engine.as_ref(), mock_table.table_root(), 0, None)
        .unwrap()
        .into_iter();

    let scanner = LogReplayScanner::prepare_table_changes(
        commits.next().unwrap(),
        engine.as_ref(),
        &get_schema().into(),
    )
    .unwrap();
    assert!(!scanner.has_cdc_action);
    assert_eq!(scanner.remove_dvs, HashMap::new());

    assert_eq!(
        result_to_sv(scanner.into_scan_batches(engine, None).unwrap()),
        &[false; 6]
    );
}

#[tokio::test]
async fn cdc_selection() {
    let engine = Arc::new(SyncEngine::new());
    let mut mock_table = LocalMockTable::new();
    mock_table
        .commit([
            Add {
                path: "fake_path_1".into(),
                data_change: true,
                ..Default::default()
            }
            .into(),
            Remove {
                path: "fake_path_2".into(),
                data_change: true,
                ..Default::default()
            }
            .into(),
            Cdc {
                path: "fake_path_3".into(),
                data_change: true,
                ..Default::default()
            }
            .into(),
        ])
        .await;

    let mut commits = get_segment(engine.as_ref(), mock_table.table_root(), 0, None)
        .unwrap()
        .into_iter();

    let scanner = LogReplayScanner::prepare_table_changes(
        commits.next().unwrap(),
        engine.as_ref(),
        &get_schema().into(),
    )
    .unwrap();
    assert!(scanner.has_cdc_action);
    assert_eq!(scanner.remove_dvs, HashMap::new());

    assert_eq!(
        result_to_sv(scanner.into_scan_batches(engine, None).unwrap()),
        &[false, false, true]
    );
}

#[tokio::test]
async fn dv() {
    let engine = Arc::new(SyncEngine::new());
    let mut mock_table = LocalMockTable::new();

    let deletion_vector1 = DeletionVectorDescriptor {
        storage_type: "u".to_string(),
        path_or_inline_dv: "vBn[lx{q8@P<9BNH/isA".to_string(),
        offset: Some(1),
        size_in_bytes: 36,
        cardinality: 2,
    };
    let deletion_vector2 = DeletionVectorDescriptor {
        storage_type: "u".to_string(),
        path_or_inline_dv: "U5OWRz5k%CFT.Td}yCPW".to_string(),
        offset: Some(1),
        size_in_bytes: 38,
        cardinality: 3,
    };
    mock_table
        .commit([
            Add {
                path: "fake_path_1".into(),
                data_change: true,
                ..Default::default()
            }
            .into(),
            Remove {
                path: "fake_path_1".into(),
                data_change: true,
                deletion_vector: Some(deletion_vector1.clone()),
                ..Default::default()
            }
            .into(),
            Remove {
                path: "fake_path_2".into(),
                data_change: true,
                deletion_vector: Some(deletion_vector2.clone()),
                ..Default::default()
            }
            .into(),
        ])
        .await;

    let mut commits = get_segment(engine.as_ref(), mock_table.table_root(), 0, None)
        .unwrap()
        .into_iter();

    let scanner = LogReplayScanner::prepare_table_changes(
        commits.next().unwrap(),
        engine.as_ref(),
        &get_schema().into(),
    )
    .unwrap();
    assert!(!scanner.has_cdc_action);
    assert_eq!(
        scanner.remove_dvs,
        HashMap::from([(
            "fake_path_1".to_string(),
            DvInfo {
                deletion_vector: Some(deletion_vector1.clone())
            }
        )])
    );

    assert_eq!(
        result_to_sv(scanner.into_scan_batches(engine, None).unwrap()),
        &[true, false, true]
    );
}
#[tokio::test]
async fn failing_protocol() {
    let engine = Arc::new(SyncEngine::new());
    let mut mock_table = LocalMockTable::new();

    let protocol = Protocol::try_new(
        3,
        1,
        ["fake_feature".to_string()].into(),
        ["fake_feature".to_string()].into(),
    )
    .unwrap();

    mock_table
        .commit([
            Add {
                path: "fake_path_1".into(),
                data_change: true,
                ..Default::default()
            }
            .into(),
            Remove {
                path: "fake_path_2".into(),
                data_change: true,
                ..Default::default()
            }
            .into(),
            protocol.into(),
        ])
        .await;

    let mut commits = get_segment(engine.as_ref(), mock_table.table_root(), 0, None)
        .unwrap()
        .into_iter();

    let res = LogReplayScanner::prepare_table_changes(
        commits.next().unwrap(),
        engine.as_ref(),
        &get_schema().into(),
    );

    assert!(res.is_err());
}

#[tokio::test]
async fn file_meta_timestamp() {
    let engine = Arc::new(SyncEngine::new());
    let mut mock_table = LocalMockTable::new();

    mock_table
        .commit([Add {
            path: "fake_path_1".into(),
            data_change: true,
            ..Default::default()
        }
        .into()])
        .await;

    let mut commits = get_segment(engine.as_ref(), mock_table.table_root(), 0, None)
        .unwrap()
        .into_iter();

    let commit = commits.next().unwrap();
    let file_meta_ts = commit.location.last_modified;
    let scanner =
        LogReplayScanner::prepare_table_changes(commit, engine.as_ref(), &get_schema().into())
            .unwrap();
    assert_eq!(scanner.timestamp, file_meta_ts);
}
