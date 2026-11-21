//! Private SQLite database behind a plugin's `serein.db_execute` import.
//!
//! Each plugin with the `database` grant owns one file in its own extension directory, so it
//! shares the directory's lifetime: disabling the plugin or logging out of its account
//! removes it. An invocation opens the file on its first request and runs every request in
//! one transaction that the host commits only after the invocation succeeds, so a failed,
//! cancelled or out-of-fuel invocation leaves the database as it was.
//!
//! Plugin SQL reaches only that file. An authorizer refuses ATTACH, DETACH, transaction
//! control (savepoints remain), temporary schema, PRAGMAs outside a read-mostly list, virtual
//! tables and extension loading. The bundled SQLite has no full-text or R-tree modules. The file is capped by page count, every
//! statement by SQLite's length and depth limits, and an invocation's total SQLite work by a
//! progress budget.
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use extensions::Reply;
use rusqlite::config::DbConfig;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, OpenFlags};
use std::fs;
use std::path::PathBuf;

/// File name inside the plugin's extension directory.
pub const FILE_NAME: &str = "data.sqlite";
/// Database size a plugin gets until the user picks another of [`QUOTAS`].
pub const DEFAULT_QUOTA: u64 = 64 * MIB;
/// Database sizes the user can choose per plugin, enforced through SQLite's page count.
pub const QUOTAS: [u64; 5] = [64 * MIB, 256 * MIB, 1024 * MIB, 4096 * MIB, 16384 * MIB];
const MIB: u64 = 1024 * 1024;
/// SQLite virtual-machine instructions between progress checks.
const PROGRESS_INTERVAL: i32 = 1_000;
/// Progress checks one invocation may use: about 100 million instructions.
const MAX_PROGRESS_CHECKS: u32 = 100_000;
/// Parameters one statement may bind.
const MAX_PARAMETERS: i32 = 256;
/// Largest reply, matching the host import's pending-reply limit.
const MAX_REPLY: usize = extensions::imports::MAX_REPLY_BYTES;

/// Pragmas a plugin may run, for schema versioning and introspection only.
const PRAGMAS: &[&str] = &[
	"user_version",
	"table_info",
	"table_xinfo",
	"table_list",
	"index_list",
	"index_info",
	"index_xinfo",
	"foreign_key_list",
	"foreign_key_check",
	"quick_check",
	"integrity_check",
];

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
	sql: String,
	#[serde(default)]
	params: Vec<serde_json::Value>,
}

/// One invocation's connection. Dropping it without [`PluginDatabase::commit`] rolls back.
pub struct PluginDatabase {
	path: PathBuf,
	quota: u64,
	connection: Option<Connection>,
	/// Set once the session can no longer commit, such as after an exhausted budget.
	broken: Option<&'static str>,
}

impl PluginDatabase {
	/// A session for the file at `path`, which may grow to `quota` bytes.
	pub fn new(path: PathBuf, quota: u64) -> Self {
		Self {
			path,
			quota,
			connection: None,
			broken: None,
		}
	}

	/// Runs one JSON request. SQL errors are returned to the plugin; they never contain
	/// host paths.
	pub fn execute(&mut self, request: &[u8]) -> Reply {
		let Ok(request) = serde_json::from_slice::<Request>(request) else {
			return Reply::Failed(
				"Request must be {\"sql\": string, \"params\": [values]} JSON".into(),
			);
		};
		let mut values = Vec::with_capacity(request.params.len());
		for param in request.params {
			match value(param) {
				Some(value) => values.push(value),
				None => {
					return Reply::Failed(
						"Parameters are null, booleans, numbers, strings or {\"blob\": base64}"
							.into(),
					);
				}
			}
		}
		if let Some(reason) = self.broken {
			return Reply::Failed(reason.into());
		}
		if self.connection.is_none() {
			match open(&self.path, self.quota) {
				Ok(connection) => self.connection = Some(connection),
				Err(reason) => {
					self.broken = Some(reason);
					return Reply::Failed(reason.into());
				}
			}
		}
		let connection = self.connection.as_ref().expect("opened above");
		let result = run(connection, &request.sql, values);
		// An exhausted budget fails every later statement, and an interrupt or a full
		// database can roll the whole transaction back; later requests would otherwise
		// run outside it and commit piecemeal.
		if connection.is_autocommit()
			|| matches!(&result, Err(Failure::Sql(error)) if error.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted))
		{
			self.broken = Some("Database work for this invocation was rolled back");
		}
		match result {
			Ok(bytes) => Reply::Ok(bytes),
			Err(Failure::Sql(error)) => Reply::Failed(sql_message(&error)),
			Err(Failure::TooLarge) => {
				Reply::Failed("Result exceeds 256 KiB; select fewer rows or columns".into())
			}
		}
	}

	/// Commits the invocation's work. Does nothing when no request opened the database.
	pub fn commit(&mut self) -> Result<(), String> {
		let Some(mut connection) = self.connection.take() else {
			return Ok(());
		};
		if let Some(reason) = self.broken {
			return Err(format!("Plugin database: {reason}"));
		}
		connection
			.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
			.and_then(|()| connection.progress_handler(0, None::<fn() -> bool>))
			.and_then(|()| connection.execute_batch("COMMIT"))
			.map_err(|_| "Plugin database could not be saved".to_string())
	}
}

enum Failure {
	Sql(rusqlite::Error),
	TooLarge,
}

impl From<rusqlite::Error> for Failure {
	fn from(error: rusqlite::Error) -> Self {
		Self::Sql(error)
	}
}

fn open(path: &std::path::Path, quota: u64) -> Result<Connection, &'static str> {
	const UNAVAILABLE: &str = "Plugin database is unavailable";
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent).map_err(|_| UNAVAILABLE)?;
	}
	// No SQLITE_OPEN_URI, so file names are never parsed as URIs with options.
	let mut connection = Connection::open_with_flags(
		path,
		OpenFlags::SQLITE_OPEN_READ_WRITE
			| OpenFlags::SQLITE_OPEN_CREATE
			| OpenFlags::SQLITE_OPEN_NO_MUTEX
			| OpenFlags::SQLITE_OPEN_NOFOLLOW,
	)
	.map_err(|_| UNAVAILABLE)?;
	configure(&mut connection, quota).map_err(|_| UNAVAILABLE)?;
	Ok(connection)
}

fn configure(connection: &mut Connection, quota: u64) -> rusqlite::Result<()> {
	connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
	connection.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
	for (limit, value) in [
		(Limit::SQLITE_LIMIT_ATTACHED, 0),
		(Limit::SQLITE_LIMIT_LENGTH, 1024 * 1024),
		(Limit::SQLITE_LIMIT_SQL_LENGTH, 100_000),
		(Limit::SQLITE_LIMIT_COLUMN, 256),
		(Limit::SQLITE_LIMIT_EXPR_DEPTH, 100),
		(Limit::SQLITE_LIMIT_COMPOUND_SELECT, 50),
		(Limit::SQLITE_LIMIT_FUNCTION_ARG, 32),
		(Limit::SQLITE_LIMIT_LIKE_PATTERN_LENGTH, 1_000),
		(Limit::SQLITE_LIMIT_VARIABLE_NUMBER, MAX_PARAMETERS),
		(Limit::SQLITE_LIMIT_TRIGGER_DEPTH, 10),
		(Limit::SQLITE_LIMIT_WORKER_THREADS, 0),
	] {
		connection.set_limit(limit, value)?;
	}
	let page_size: i64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
	connection.execute_batch(&format!(
		"PRAGMA max_page_count = {}; PRAGMA cache_size = -2048; PRAGMA foreign_keys = ON;
		 BEGIN IMMEDIATE",
		quota / page_size.max(512) as u64
	))?;
	let mut checks = 0_u32;
	connection.progress_handler(
		PROGRESS_INTERVAL,
		Some(move || {
			checks += 1;
			checks > MAX_PROGRESS_CHECKS
		}),
	)?;
	connection.authorizer(Some(authorize))
}

fn authorize(context: AuthContext<'_>) -> Authorization {
	if context.database_name.is_some_and(|name| name != "main") {
		return Authorization::Deny;
	}
	let allowed = match context.action {
		AuthAction::Pragma { pragma_name, .. } => PRAGMAS
			.iter()
			.any(|allowed| allowed.eq_ignore_ascii_case(pragma_name)),
		AuthAction::Function { function_name } => !["load_extension", "fts3_tokenizer"]
			.iter()
			.any(|denied| denied.eq_ignore_ascii_case(function_name)),
		AuthAction::CreateIndex { .. }
		| AuthAction::CreateTable { .. }
		| AuthAction::CreateTrigger { .. }
		| AuthAction::CreateView { .. }
		| AuthAction::Delete { .. }
		| AuthAction::DropIndex { .. }
		| AuthAction::DropTable { .. }
		| AuthAction::DropTrigger { .. }
		| AuthAction::DropView { .. }
		| AuthAction::Insert { .. }
		| AuthAction::Read { .. }
		| AuthAction::Select
		| AuthAction::Update { .. }
		| AuthAction::AlterTable { .. }
		| AuthAction::Reindex { .. }
		| AuthAction::Analyze { .. }
		| AuthAction::Savepoint { .. }
		| AuthAction::Recursive => true,
		// Attach, detach, transaction control, temporary schema, virtual tables and
		// unknown actions.
		_ => false,
	};
	if allowed {
		Authorization::Allow
	} else {
		Authorization::Deny
	}
}

fn value(json: serde_json::Value) -> Option<Value> {
	Some(match json {
		serde_json::Value::Null => Value::Null,
		serde_json::Value::Bool(value) => Value::Integer(i64::from(value)),
		serde_json::Value::Number(number) => match number.as_i64() {
			Some(integer) => Value::Integer(integer),
			None if number.is_f64() => Value::Real(number.as_f64()?),
			None => return None,
		},
		serde_json::Value::String(text) => Value::Text(text),
		serde_json::Value::Object(object) => match object.get("blob") {
			Some(serde_json::Value::String(encoded)) if object.len() == 1 => {
				Value::Blob(BASE64.decode(encoded).ok()?)
			}
			_ => return None,
		},
		serde_json::Value::Array(_) => return None,
	})
}

fn json(value: ValueRef<'_>) -> serde_json::Value {
	match value {
		ValueRef::Null => serde_json::Value::Null,
		ValueRef::Integer(integer) => integer.into(),
		ValueRef::Real(real) => serde_json::Number::from_f64(real)
			.map_or(serde_json::Value::Null, serde_json::Value::Number),
		ValueRef::Text(text) => String::from_utf8_lossy(text).into_owned().into(),
		ValueRef::Blob(blob) => serde_json::json!({ "blob": BASE64.encode(blob) }),
	}
}

/// Runs one statement, writing `{"columns", "rows", "changes", "last_insert_rowid"}`.
fn run(connection: &Connection, sql: &str, values: Vec<Value>) -> Result<Vec<u8>, Failure> {
	let mut statement = connection.prepare(sql)?;
	let columns: Vec<String> = statement.column_names().map(Into::into).collect();
	let writes = !statement.readonly();
	let mut out = br#"{"columns":"#.to_vec();
	serde_json::to_writer(&mut out, &columns).map_err(|_| Failure::TooLarge)?;
	out.extend_from_slice(br#","rows":["#);
	let mut rows = statement.query(rusqlite::params_from_iter(values))?;
	let mut first = true;
	while let Some(row) = rows.next()? {
		if !std::mem::take(&mut first) {
			out.push(b',');
		}
		let cells: Vec<serde_json::Value> = (0..columns.len())
			.map(|index| row.get_ref(index).map(json))
			.collect::<rusqlite::Result<_>>()?;
		serde_json::to_writer(&mut out, &cells).map_err(|_| Failure::TooLarge)?;
		if out.len() > MAX_REPLY {
			return Err(Failure::TooLarge);
		}
	}
	drop(rows);
	let (changes, rowid) = if writes {
		(connection.changes(), connection.last_insert_rowid())
	} else {
		(0, 0)
	};
	out.extend_from_slice(
		format!(r#"],"changes":{changes},"last_insert_rowid":{rowid}}}"#).as_bytes(),
	);
	if out.len() > MAX_REPLY {
		return Err(Failure::TooLarge);
	}
	Ok(out)
}

fn sql_message(error: &rusqlite::Error) -> String {
	match error {
		rusqlite::Error::SqliteFailure(code, _)
			if code.code == rusqlite::ErrorCode::OperationInterrupted =>
		{
			"Database work exceeded this invocation's budget".into()
		}
		rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::DiskFull => {
			"Plugin database is full; the user can raise its size limit in Extensions".into()
		}
		rusqlite::Error::SqliteFailure(_, Some(message)) => message.clone(),
		rusqlite::Error::MultipleStatement => "Send one statement per request".into(),
		rusqlite::Error::InvalidParameterCount(given, expected) => {
			format!("Statement takes {expected} parameters; {given} given")
		}
		_ => "Database request failed".into(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	struct Directory(PathBuf);
	impl Directory {
		fn new() -> Self {
			let mut bytes = [0; 8];
			getrandom::fill(&mut bytes).unwrap();
			let path = std::env::temp_dir().join(format!(
				"serein-plugin-db-{}",
				bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
			));
			Self(path)
		}
		fn database(&self) -> PluginDatabase {
			PluginDatabase::new(self.0.join("plugin").join(FILE_NAME), DEFAULT_QUOTA)
		}
	}
	impl Drop for Directory {
		fn drop(&mut self) {
			let _ = fs::remove_dir_all(&self.0);
		}
	}

	fn exec(
		db: &mut PluginDatabase,
		sql: &str,
		params: serde_json::Value,
	) -> Result<serde_json::Value, String> {
		let request =
			serde_json::to_vec(&serde_json::json!({ "sql": sql, "params": params })).unwrap();
		match db.execute(&request) {
			Reply::Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap()),
			Reply::Failed(message) => Err(message),
			Reply::Unavailable => Err("unavailable".into()),
		}
	}

	#[test]
	fn work_commits_only_when_the_host_commits_and_values_round_trip() {
		let directory = Directory::new();
		let mut db = directory.database();
		exec(
			&mut db,
			"CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT, raw BLOB, score REAL)",
			serde_json::json!([]),
		)
		.unwrap();
		let inserted = exec(
			&mut db,
			"INSERT INTO notes (body, raw, score) VALUES (?, ?, ?)",
			serde_json::json!(["hi", { "blob": "AAH/" }, 1.5]),
		)
		.unwrap();
		assert_eq!(inserted["changes"], 1);
		assert_eq!(inserted["last_insert_rowid"], 1);
		db.commit().unwrap();

		// An invocation that is never committed leaves no trace.
		let mut db = directory.database();
		exec(
			&mut db,
			"INSERT INTO notes (body) VALUES ('lost')",
			serde_json::json!([]),
		)
		.unwrap();
		drop(db);

		let mut db = directory.database();
		let rows = exec(
			&mut db,
			"SELECT body, raw, score FROM notes",
			serde_json::json!([]),
		)
		.unwrap();
		assert_eq!(rows["columns"], serde_json::json!(["body", "raw", "score"]));
		assert_eq!(
			rows["rows"],
			serde_json::json!([["hi", { "blob": "AAH/" }, 1.5]])
		);
		assert_eq!(rows["changes"], 0);
	}

	#[test]
	fn sql_cannot_reach_other_files_transactions_or_settings() {
		let directory = Directory::new();
		let outside = directory.0.join("outside.sqlite");
		let mut db = directory.database();
		exec(&mut db, "CREATE TABLE t (x)", serde_json::json!([])).unwrap();
		let path = outside.to_string_lossy().replace('\'', "''");
		for sql in [
			format!("ATTACH DATABASE '{path}' AS other"),
			format!("VACUUM INTO '{path}'"),
			"VACUUM".into(),
			"COMMIT".into(),
			"BEGIN".into(),
			"ROLLBACK".into(),
			"PRAGMA max_page_count = 1000000".into(),
			"PRAGMA journal_mode = OFF".into(),
			"PRAGMA writable_schema = ON".into(),
			"CREATE TEMP TABLE scratch (x)".into(),
			"CREATE VIRTUAL TABLE r USING rtree(id, a, b)".into(),
			"SELECT * FROM pragma_function_list".into(),
			"SELECT load_extension('anything')".into(),
			"UPDATE sqlite_schema SET sql = '' WHERE name = 't'".into(),
			"SELECT 1; SELECT 2".into(),
		] {
			assert!(exec(&mut db, &sql, serde_json::json!([])).is_err(), "{sql}");
		}
		assert!(!outside.exists());
		// Savepoints, versioning and introspection remain available.
		for sql in [
			"SAVEPOINT step",
			"INSERT INTO t VALUES (1)",
			"RELEASE step",
			"PRAGMA user_version = 3",
			"SELECT name FROM pragma_table_info('t')",
		] {
			exec(&mut db, sql, serde_json::json!([])).unwrap();
		}
		db.commit().unwrap();
		let mut db = directory.database();
		assert_eq!(
			exec(&mut db, "PRAGMA user_version", serde_json::json!([])).unwrap()["rows"],
			serde_json::json!([[3]])
		);
	}

	#[test]
	fn budgets_bound_work_size_and_replies() {
		let directory = Directory::new();
		let mut db = directory.database();
		exec(&mut db, "CREATE TABLE t (x)", serde_json::json!([])).unwrap();
		exec(&mut db, "INSERT INTO t VALUES (1)", serde_json::json!([])).unwrap();
		assert!(exec(&mut db, "SELECT ?", serde_json::json!([1, 2])).is_err());
		assert!(exec(&mut db, "SELECT ?", serde_json::json!([[1]])).is_err());
		assert!(
			exec(
				&mut db,
				"SELECT zeroblob(200000), zeroblob(200000)",
				serde_json::json!([])
			)
			.unwrap_err()
			.contains("256 KiB")
		);
		// Runaway SQL is interrupted, which rolls back the whole invocation.
		let spin = "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) SELECT count(*) FROM n";
		assert!(
			exec(&mut db, spin, serde_json::json!([]))
				.unwrap_err()
				.contains("budget")
		);
		assert!(exec(&mut db, "SELECT 1", serde_json::json!([])).is_err());
		assert!(db.commit().is_err());
		let mut db = directory.database();
		assert!(exec(&mut db, "SELECT * FROM t", serde_json::json!([])).is_err());
		drop(db);

		// The file cannot outgrow its page budget.
		let mut db = directory.database();
		exec(&mut db, "CREATE TABLE big (x)", serde_json::json!([])).unwrap();
		let fill = "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 80) INSERT INTO big SELECT zeroblob(1000000) FROM n";
		assert!(
			exec(&mut db, fill, serde_json::json!([]))
				.unwrap_err()
				.contains("full")
		);
	}
}
