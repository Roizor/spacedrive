use crate::{
	job::{JobError, JobReportUpdate, WorkerContext},
	library::LibraryContext,
	prisma::{file_path, location},
	sync,
};

use std::{
	ffi::OsStr,
	hash::{Hash, Hasher},
	path::{Path, PathBuf},
	time::Duration,
};

use chrono::{DateTime, Utc};
use int_enum::IntEnumError;
use rmp_serde::{decode, encode};
use rspc::ErrorCode;
use rules::RuleKind;
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use tokio::{fs, io};
use tracing::info;

use super::{
	file_path_helper::{get_parent_dir, FilePathError},
	LocationId,
};

pub mod indexer_job;
pub mod rules;
pub mod shallow_indexer_job;
mod walk;

location::include!(indexer_job_location {
	indexer_rules: select { indexer_rule }
});

/// `IndexerJobInit` receives a `location::Data` object to be indexed
/// and possibly a `sub_path` to be indexed. The `sub_path` is used when
/// we want do index just a part of a location.
#[derive(Serialize, Deserialize)]
pub struct IndexerJobInit {
	pub location: indexer_job_location::Data,
	pub sub_path: Option<PathBuf>,
}

impl Hash for IndexerJobInit {
	fn hash<H: Hasher>(&self, state: &mut H) {
		self.location.id.hash(state);
		if let Some(ref sub_path) = self.sub_path {
			sub_path.hash(state);
		}
	}
}
/// `IndexerJobData` contains the state of the indexer job, which includes a `location_path` that
/// is cached and casted on `PathBuf` from `local_path` column in the `location` table. It also
/// contains some metadata for logging purposes.
#[derive(Serialize, Deserialize)]
pub struct IndexerJobData {
	db_write_start: DateTime<Utc>,
	scan_read_time: Duration,
	total_paths: usize,
	indexed_paths: i64,
}

/// `IndexerJobStep` is a type alias, specifying that each step of the [`IndexerJob`] is a vector of
/// `IndexerJobStepEntry`. The size of this vector is given by the [`BATCH_SIZE`] constant.
pub type IndexerJobStep = Vec<IndexerJobStepEntry>;

/// `IndexerJobStepEntry` represents a single file to be indexed, given its metadata to be written
/// on the `file_path` table in the database
#[derive(Serialize, Deserialize)]
pub struct IndexerJobStepEntry {
	path: PathBuf,
	created_at: DateTime<Utc>,
	file_id: i32,
	parent_id: Option<i32>,
	is_dir: bool,
}

impl IndexerJobData {
	fn on_scan_progress(ctx: &WorkerContext, progress: Vec<ScanProgress>) {
		ctx.progress_debounced(
			progress
				.iter()
				.map(|p| match p.clone() {
					ScanProgress::ChunkCount(c) => JobReportUpdate::TaskCount(c),
					ScanProgress::SavedChunks(p) => JobReportUpdate::CompletedTaskCount(p),
					ScanProgress::Message(m) => JobReportUpdate::Message(m),
				})
				.collect(),
		)
	}
}

#[derive(Clone)]
pub enum ScanProgress {
	ChunkCount(usize),
	SavedChunks(usize),
	Message(String),
}

/// Error type for the indexer module
#[derive(Error, Debug)]
pub enum IndexerError {
	// Not Found errors
	#[error("Indexer rule not found: <id={0}>")]
	IndexerRuleNotFound(i32),

	// User errors
	#[error("Invalid indexer rule kind integer: {0}")]
	InvalidRuleKindInt(#[from] IntEnumError<RuleKind>),
	#[error("Glob builder error: {0}")]
	GlobBuilderError(#[from] globset::Error),
	#[error("Received an invalid sub path: <location_path={location_path}, sub_path={sub_path}>")]
	InvalidSubPath {
		location_path: PathBuf,
		sub_path: PathBuf,
	},
	#[error("Sub path is not a directory: {0}")]
	SubPathNotDirectory(PathBuf),
	#[error("The parent directory of the received sub path isn't indexed in the location: <id={location_id}, sub_path={sub_path}>")]
	SubPathParentNotInLocation {
		location_id: LocationId,
		sub_path: PathBuf,
	},

	// Internal Errors
	#[error("Database error: {0}")]
	DatabaseError(#[from] prisma_client_rust::QueryError),
	#[error("I/O error: {0}")]
	IOError(#[from] io::Error),
	#[error("Indexer rule parameters json serialization error: {0}")]
	RuleParametersSerdeJson(#[from] serde_json::Error),
	#[error("Indexer rule parameters encode error: {0}")]
	RuleParametersRMPEncode(#[from] encode::Error),
	#[error("Indexer rule parameters decode error: {0}")]
	RuleParametersRMPDecode(#[from] decode::Error),
	#[error("File path related error (error: {0})")]
	FilePathError(#[from] FilePathError),
}

impl From<IndexerError> for rspc::Error {
	fn from(err: IndexerError) -> Self {
		match err {
			IndexerError::IndexerRuleNotFound(_) => {
				rspc::Error::with_cause(ErrorCode::NotFound, err.to_string(), err)
			}

			IndexerError::InvalidRuleKindInt(_) | IndexerError::GlobBuilderError(_) => {
				rspc::Error::with_cause(ErrorCode::BadRequest, err.to_string(), err)
			}

			_ => rspc::Error::with_cause(ErrorCode::InternalServerError, err.to_string(), err),
		}
	}
}

/// Extract name from OsStr returned by PathBuff
fn extract_name(os_string: Option<&OsStr>) -> String {
	os_string
		.unwrap_or_default()
		.to_str()
		.unwrap_or_default()
		.to_owned()
}

fn ensure_sub_path_is_in_location(
	location_path: impl AsRef<Path>,
	sub_path: impl AsRef<Path>,
) -> Result<(), IndexerError> {
	let sub_path = sub_path.as_ref();
	let location_path = location_path.as_ref();

	if !sub_path.starts_with(location_path) {
		Err(IndexerError::InvalidSubPath {
			sub_path: sub_path.to_path_buf(),
			location_path: location_path.to_path_buf(),
		})
	} else {
		Ok(())
	}
}

async fn ensure_sub_path_is_directory(sub_path: impl AsRef<Path>) -> Result<(), IndexerError> {
	let sub_path = sub_path.as_ref();

	if fs::metadata(sub_path).await?.is_file() {
		Err(IndexerError::SubPathNotDirectory(sub_path.to_path_buf()))
	} else {
		Ok(())
	}
}

async fn ensure_sub_path_parent_is_in_location(
	location_id: LocationId,
	sub_path: impl AsRef<Path>,
	library_ctx: &LibraryContext,
) -> Result<file_path::Data, IndexerError> {
	let sub_path = sub_path.as_ref();
	get_parent_dir(location_id, sub_path, library_ctx)
		.await?
		.ok_or_else(|| IndexerError::SubPathParentNotInLocation {
			sub_path: sub_path.to_path_buf(),
			location_id,
		})
}

async fn execute_indexer_step(
	location: &indexer_job_location::Data,
	step: &[IndexerJobStepEntry],
	ctx: WorkerContext,
) -> Result<i64, JobError> {
	let db = &ctx.library_ctx.db;

	let (sync_stuff, paths): (Vec<_>, Vec<_>) = step
		.iter()
		.map(|entry| {
			let name;
			let extension;

			// if 'entry.path' is a directory, set extension to an empty string to
			// avoid periods in folder names being interpreted as file extensions
			if entry.is_dir {
				extension = None;
				name = extract_name(entry.path.file_name());
			} else {
				// if the 'entry.path' is not a directory, then get the extension and name.
				extension = Some(extract_name(entry.path.extension()).to_lowercase());
				name = extract_name(entry.path.file_stem());
			}
			let mut materialized_path = entry
				.path
				.strip_prefix(&location.path)
				.unwrap()
				.to_str()
				.expect("Found non-UTF-8 path")
				.to_string();

			if entry.is_dir && !materialized_path.ends_with('/') {
				materialized_path += "/";
			}

			use file_path::*;

			(
				(
					sync::file_path::SyncId {
						id: entry.file_id,
						location: sync::location::SyncId {
							pub_id: location.pub_id.clone(),
						},
					},
					[
						("materialized_path", json!(materialized_path.clone())),
						("name", json!(name.clone())),
						("is_dir", json!(entry.is_dir)),
						("extension", json!(extension.clone())),
						("parent_id", json!(entry.parent_id)),
						("date_created", json!(entry.created_at)),
					],
				),
				file_path::create_unchecked(
					entry.file_id,
					location.id,
					materialized_path,
					name,
					vec![
						is_dir::set(entry.is_dir),
						extension::set(extension),
						parent_id::set(entry.parent_id),
						date_created::set(entry.created_at.into()),
					],
				),
			)
		})
		.unzip();

	let count = ctx
		.library_ctx
		.sync
		.write_op(
			db,
			ctx.library_ctx.sync.owned_create_many(sync_stuff, true),
			db.file_path().create_many(paths).skip_duplicates(),
		)
		.await?;

	info!("Inserted {count} records");

	Ok(count)
}

#[macro_export]
#[allow(clippy::crate_in_macro_def)]
macro_rules! finalize_indexer {
	($state:ident, $ctx:ident) => {{
		let data = $state
			.data
			.as_ref()
			.expect("critical error: missing data on job state");

		tracing::info!(
			"scan of {} completed in {:?}. {} files found. db write completed in {:?}",
			$state.init.location.path,
			data.scan_read_time,
			data.total_paths,
			(Utc::now() - data.db_write_start)
				.to_std()
				.expect("critical error: non-negative duration"),
		);

		// TODO: Currently there is a problem in file_path table, where unique constraints aren't 
		// being respected due to `extension` being nullable, when fixed uncomment this if
		// if data.indexed_paths > 0 {
		// 	crate::invalidate_query!($ctx.library_ctx, "locations.getExplorerData");
		// }

		Ok(Some(serde_json::to_value($state)?))
	}};
}

impl From<indexer_job_location::Data> for location::Data {
	fn from(indexer_job_location: indexer_job_location::Data) -> Self {
		Self {
			id: indexer_job_location.id,
			pub_id: indexer_job_location.pub_id,
			path: indexer_job_location.path,
			node_id: indexer_job_location.node_id,
			name: indexer_job_location.name,
			total_capacity: indexer_job_location.total_capacity,
			available_capacity: indexer_job_location.available_capacity,
			is_archived: indexer_job_location.is_archived,
			generate_preview_media: indexer_job_location.generate_preview_media,
			sync_preview_media: indexer_job_location.sync_preview_media,
			hidden: indexer_job_location.hidden,
			date_created: indexer_job_location.date_created,
			node: None,
			file_paths: None,
			indexer_rules: None,
		}
	}
}
