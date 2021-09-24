//! Handle incoming HTTP requests.
#![allow(unknown_lints)]
use futures::executor::block_on;
use std::cmp;
use std::collections::HashMap;
use std::{thread, time};

use crate::auth::{AuthType, FxAAuthenticator};
use crate::config::ServerConfig;
use crate::db::{self, models::DatabaseManager, Conn, MysqlPool};
use crate::error::{HandlerError, HandlerErrorKind, HandlerResult};
use crate::logging::RBLogger;
use crate::metrics::Metrics;
use crate::sqs::{self, SyncEvent};
use crate::tags::Tags;

use rocket::config;
use rocket::fairing::AdHoc;
use rocket::http::{Method, RawStr, Status};
use rocket::request::{self, FromRequest};
use rocket::response::{content, status};
use rocket::Outcome::Success;
use rocket::{self, Request};
use rocket_contrib::{
    json,
    json::{Json, JsonValue},
};
use serde_derive::Deserialize;
use slog::{debug, error, info};

/// An incoming Data Storage request.
#[derive(Deserialize, Debug)]
pub struct DataRecord {
    /// The expected time to live for a given record
    ttl: u64,
    // The serialized data to store.
    data: String,
}

// Collect header info.Request
pub struct HeaderInfo {
    headers: HashMap<String, Option<String>>,
}

impl<'a, 'r> FromRequest<'a, 'r> for HeaderInfo {
    type Error = HandlerError;

    fn from_request(request: &'a Request<'r>) -> request::Outcome<Self, HandlerError> {
        let mut headers: HashMap<String, Option<String>> = HashMap::new();
        headers.insert(
            "request_id".to_owned(),
            request
                .headers()
                .get_one("FxA-Request-Id")
                .or_else(|| request.headers().get_one("X-Request-Id"))
                .map(std::borrow::ToOwned::to_owned),
        );
        Success(HeaderInfo { headers })
    }
}

// Encapsulate the server.
pub struct Server {}

impl Server {
    fn process_message(pool: &MysqlPool, event: &SyncEvent) -> Result<(), HandlerError> {
        let conn = &pool
            .get()
            .map_err(|e| HandlerErrorKind::GeneralError(e.to_string()))?;
        db::models::DatabaseManager::delete(conn, &event.uid, &event.id)?;
        Ok(())
    }

    /// Initialize the server and prepare it for running. This will run any r2d2 embedded
    /// migrations required, create various guarded pool objects, and start the SQS handler
    /// (unless we're testing)
    pub fn start(rocket: rocket::Rocket) -> Result<rocket::Rocket, HandlerError> {
        // tests on circle can re-run the embedded migrations. this can fail because of
        // index recreations.
        match db::run_embedded_migrations(rocket.config()) {
            Ok(_) => {}
            Err(e) => {
                if cfg!(test) {
                    dbg!("Encountered possible error: {:?}", e);
                } else {
                    return Err(e);
                }
            }
        };

        let db_pool = db::pool_from_config(rocket.config()).expect("Could not get pool");
        let sqs_config = rocket.config().clone();
        let sq_logger = RBLogger::new(rocket.config());
        if !cfg!(test) {
            // if we're not running a test, spawn the SQS handler for account/device deletes
            thread::spawn(move || {
                debug!(sq_logger.log, "SQS Starting thread... ");
                let sqs_handler = sqs::SyncEventQueue::from_config(&sqs_config, &sq_logger);
                loop {
                    if let Some(event) = block_on(sqs_handler.fetch()) {
                        if let Err(e) = Server::process_message(&db_pool, &event) {
                            error!(sq_logger.log, "Could not process message"; "error" => e.to_string());
                        };
                    } else {
                        // sleep 5m
                        thread::sleep(time::Duration::from_secs(300));
                    }
                }
            });
        }

        Ok(rocket
            .attach(AdHoc::on_attach("init", |rocket| {
                // Copy the config into a state manager.
                let pool = db::pool_from_config(rocket.config()).expect("Could not get pool");
                let rbconfig = ServerConfig::new(rocket.config());
                let logger = RBLogger::new(rocket.config());
                let metrics = Metrics::init(rocket.config()).expect("Could not init metrics");
                info!(logger.log, "sLogging initialized...");
                Ok(rocket
                    .manage(rbconfig)
                    .manage(pool)
                    .manage(logger)
                    .manage(metrics))
            }))
            .mount("/v1/store", routes![read, write, delete, delete_user])
            .mount("/", routes![version, heartbeat, lbheartbeat]))
    }
}

/// Check an Authorization token permission, defaulting to the proper schema check.
pub fn check_token(
    config: &ServerConfig,
    method: Method,
    device_id: &str,
    token: &HandlerResult<FxAAuthenticator>,
) -> Result<bool, HandlerError> {
    match token {
        Ok(token) => match token.auth_type {
            AuthType::FxAServer => check_server_token(config, method, device_id, token),
            AuthType::FxAOauth => check_fxa_token(config, method, device_id, token),
        },
        Err(_e) => Err(HandlerErrorKind::UnauthorizedBadToken.into()),
    }
}

/// Stub for FxA server token permission authentication.
///
/// The Auth module actually checks the Authorization header for this value and no further
/// processing is required. If additional processing of the token value should be necessary,
/// place it here.
pub fn check_server_token(
    _config: &ServerConfig,
    _method: Method,
    _device_id: &str,
    _token: &FxAAuthenticator,
) -> Result<bool, HandlerError> {
    // currently a stub for the FxA server token auth.
    // In theory, the auth mod already checks the token against config.
    Ok(true)
}

/// Check the permissions of the FxA token to see if read/write access is provided.
///
/// auth::FxAAuthenticator::from_fxa_oauth() does a general FxA validation check, but
/// cannot check scopes for various reasons. Check the returned scopes here to finalize
/// authorization.
pub fn check_fxa_token(
    config: &ServerConfig,
    method: Method,
    device_id: &str,
    token: &FxAAuthenticator,
) -> Result<bool, HandlerError> {
    // call unwrap here because we already checked for instances.
    let scope = &token.scope;
    if scope.contains(&FxAAuthenticator::fxa_root(&config.auth_app_name)) {
        return Ok(true);
    }
    // Otherwise check for explicit allowances
    match method {
        Method::Put | Method::Post | Method::Delete => {
            if scope.contains(&format!(
                "{}send/{}",
                FxAAuthenticator::fxa_root(&config.auth_app_name),
                device_id
            )) || scope.contains(&format!(
                "{}send",
                FxAAuthenticator::fxa_root(&config.auth_app_name)
            )) {
                return Ok(true);
            }
        }
        Method::Get => {
            if scope.contains(&format!(
                "{}recv/{}",
                FxAAuthenticator::fxa_root(&config.auth_app_name),
                device_id
            )) || scope.contains(&format!(
                "{}recv",
                FxAAuthenticator::fxa_root(&config.auth_app_name)
            )) {
                return Ok(true);
            }
        }
        _ => {}
    }
    Err(HandlerErrorKind::UnauthorizedBadToken.into())
}

/// ## GET method handler (with options)
///
/// Process a `GET /<user>/<device>` request and return appropriate records.
///
/// Accepts the optional query parameters:
/// index: The start index to begin querying from
/// limit: The max number of records to return for this request.
/// status: The status of the client:
///         **lost** - Client just needs to know the latest index.
///         **new**  - Client needs latest index and all available records.
#[allow(clippy::too_many_arguments)]
#[get("/<user_id>/<device_id>?<index>&<limit>&<status>")]
fn read<'r>(
    conn: Conn,
    config: ServerConfig,
    logger: RBLogger,
    headers: HeaderInfo,
    token: HandlerResult<FxAAuthenticator>,
    user_id: String,
    device_id: String,
    index: Option<Result<u64, &'r RawStr>>,
    limit: Option<Result<u64, &'r RawStr>>,
    status: Option<Result<String, &'r RawStr>>,
    metrics: Metrics,
) -> Result<JsonValue, HandlerError> {
    let mut index = index
        .transpose()
        .map_err(|e| HandlerErrorKind::InvalidOptionIndex(e.to_string()))?;
    let mut limit = limit
        .transpose()
        .map_err(|e| HandlerErrorKind::InvalidOptionLimit(e.to_string()))?;
    let status = status
        .transpose()
        .map_err(|e| HandlerErrorKind::InvalidOptionLimit(e.to_string()))?;
    // 👩🏫 note that the "token" var is a HandlerResult wrapped Validate struct.
    // Validate::from_request extracts the token from the Authorization header, validates it
    // against FxA and the method, and either returns OK or an error. We need to reraise it to the
    // handler.
    let request_id = headers.headers.get("request_id");

    let mut tags = Tags::default();
    tags.extra.insert("user_id".to_owned(), user_id.clone());
    tags.extra.insert("device_id".to_owned(), device_id.clone());
    tags.extra
        .insert("request_id".to_owned(), format!("{:?}", request_id));
    tags.tags
        .insert("status".to_owned(), status.clone().unwrap_or_default());

    debug!(logger.log, "Handling Read";
           "user_id" => &user_id,
           "device_id" => &device_id,
           "request_id" => &request_id,
    );
    check_token(&config, Method::Get, &device_id, &token)?;
    let max_index = DatabaseManager::max_index(&conn, &user_id, &device_id)?;
    match status
        .unwrap_or_else(|| String::from(""))
        .to_lowercase()
        .as_str()
    {
        "new" => {
            // New entry, needs all data
            index = None;
            limit = None;
            debug!(logger.log, "Welcome new user";
                   "user_id" => &user_id,
                   "request_id" => &request_id
            );
        }
        "lost" => {
            // Just lost, needs just the next index.
            index = None;
            limit = Some(0);
            debug!(logger.log, "Sorry, you're lost";
                   "user_id" => &user_id,
                   "request_id" => &request_id
            );
        }
        _ => {}
    };
    let messages =
        DatabaseManager::read_records(&conn, &user_id, &device_id, &index, &limit).unwrap();
    let mut msg_max: u64 = 0;
    tags.extra
        .insert("msg_count".to_owned(), messages.len().to_string());
    for message in &messages {
        msg_max = cmp::max(msg_max, message.idx as u64);
    }
    debug!(logger.log, "Found messages";
           "len" => messages.len(),
           "user_id" => &user_id,
           "request_id" => &request_id
    );
    // returns json {"status":200, "index": max_index, "messages":[{"index": #, "data": String}, ...]}
    let is_last = match limit {
        None => true,
        Some(0) => true,
        Some(_) => messages
            .last()
            .map(|last| (last.idx as u64) == max_index)
            .unwrap_or(true),
    };
    metrics.incr_with_tags("pushbox.cmd.get", Some(tags));
    Ok(json!({
        "last": is_last,
        "index": msg_max,
        "status": 200,
        "messages": messages
    }))
}

/// ## POST method handler
///
/// Process a `POST /<user>/<device>` request containing a JSON body
/// ```
/// {"ttl"=TimeToLiveInteger, "data"="base64EncryptedDataBlob"}
/// ```
///
#[allow(clippy::too_many_arguments)]
#[post("/<user_id>/<device_id>", data = "<data>")]
fn write(
    conn: Conn,
    config: ServerConfig,
    logger: RBLogger,
    headers: HeaderInfo,
    token: HandlerResult<FxAAuthenticator>,
    user_id: String,
    device_id: String,
    data: Json<DataRecord>,
    metrics: Metrics,
) -> Result<JsonValue, HandlerError> {
    let request_id = headers.headers.get("request_id");

    let mut tags = Tags::default();
    tags.extra.insert("user_id".to_owned(), user_id.clone());
    tags.extra.insert("device_id".to_owned(), device_id.clone());
    tags.extra
        .insert("request_id".to_owned(), format!("{:?}", request_id));

    check_token(&config, Method::Post, &device_id, &token)?;
    if config
        .test_data
        .get("auth_only")
        .unwrap_or(&config::Value::from(false))
        .as_bool()
        .unwrap_or(false)
    {
        // Auth testing, do not write to db.
        info!(logger.log, "Auth Skipping database check.");
        return Ok(json!({
            "status": 200,
            "index": -1,
        }));
    }
    debug!(logger.log, "Writing new record:";
           "user_id" => &user_id,
           "device_id" => &device_id,
           "request_id" => &request_id
    );
    let response = DatabaseManager::new_record(
        &conn,
        &user_id,
        &device_id,
        &data.data,
        db::models::calc_ttl(data.ttl),
    );
    if response.is_err() {
        return Err(response.err().unwrap());
    }
    metrics.incr_with_tags("pushbox.cmd.post", Some(tags));
    // returns json {"status": 200, "index": #}
    Ok(json!({
        "status": 200,
        "index": response.unwrap(),
    }))
}

/// ## DELETE method handler for user and device data
///
/// Process a `DELETE /<user>/<device>` request which removes all records for a given
/// user and device.
#[delete("/<user_id>/<device_id>")]
fn delete(
    conn: Conn,
    config: ServerConfig,
    token: HandlerResult<FxAAuthenticator>,
    user_id: String,
    device_id: String,
    metrics: Metrics,
) -> Result<JsonValue, HandlerError> {
    let mut tags = Tags::default();
    tags.extra.insert("user_id".to_owned(), user_id.clone());
    tags.extra.insert("device_id".to_owned(), device_id.clone());
    check_token(&config, Method::Delete, &device_id, &token)?;
    DatabaseManager::delete(&conn, &user_id, &device_id)?;
    metrics.incr_with_tags("pushbox.cmd.del", Some(tags));
    // returns an empty object
    Ok(json!({}))
}

/// ## DELETE method handler for all user data
///
/// Process a `DELETE /<user>` request whice removes all records for a given user.
#[delete("/<user_id>")]
fn delete_user(
    conn: Conn,
    config: ServerConfig,
    token: HandlerResult<FxAAuthenticator>,
    user_id: String,
    metrics: Metrics,
) -> Result<JsonValue, HandlerError> {
    check_token(&config, Method::Delete, &String::from(""), &token)?;
    DatabaseManager::delete(&conn, &user_id, &String::from(""))?;
    // returns an empty object
    metrics.incr("pushbox.cmd.del_user");
    Ok(json!({}))
}

/// ## GET Dockerflow checks
#[get("/__version__")]
fn version() -> content::Json<&'static str> {
    content::Json(include_str!("../version.json"))
}

#[get("/__heartbeat__")]
fn heartbeat(conn: Conn, config: ServerConfig, logger: RBLogger) -> status::Custom<JsonValue> {
    let status = if let Err(e) = db::health_check(&*conn) {
        let status = Status::ServiceUnavailable;
        error!(logger.log, "Database heartbeat failed {}", e; "code" => status.code);
        status
    } else {
        Status::Ok
    };
    // XXX: maybe add a health check for SQS

    let msg = if status == Status::Ok { "ok" } else { "error" };
    status::Custom(
        status,
        json!({
            "status": msg,
            "code": status.code,
            "fxa_auth": config.fxa_host,
            "database": msg,
        }),
    )
}

#[get("/__lbheartbeat__")]
fn lbheartbeat() {}

#[cfg(test)]
mod test {
    use rand::{distributions, thread_rng, Rng};
    use std::env;

    use rocket;
    use rocket::config::{Config, Environment, RocketConfig, Table};
    use rocket::http::Header;
    use rocket::local::Client;
    use rocket_contrib::json::JsonValue;
    use serde_derive::Deserialize;
    use serde_json::Value;

    use super::Server;
    use crate::auth::FxAAuthenticator;

    #[derive(Debug, Deserialize)]
    struct WriteResp {
        index: u64,
    }

    #[derive(Debug, Deserialize)]
    struct Msg {
        index: u64,
    }

    #[derive(Debug, Deserialize)]
    struct ReadResp {
        status: u32,
        index: u64,
        messages: Vec<Msg>,
    }

    fn rocket_config(test_data: Table) -> Config {
        let rconfig = RocketConfig::read().expect("failed to read config");
        let fxa_host = rconfig
            .active()
            .get_str("fxa_host")
            .unwrap_or("oauth.stage.mozaws.net");

        let db_url = env::var("ROCKET_DATABASE_URL")
            .unwrap_or_else(|_| String::from("mysql://test:test@localhost/pushbox"));
        Config::build(Environment::Development)
            .extra("fxa_host", fxa_host)
            .extra("database_url", db_url)
            .extra("dryrun", true)
            .extra("auth_app_name", "pushbox")
            .extra("test_data", test_data)
            .finalize()
            .unwrap()
    }

    fn rocket_client(config: Config) -> Client {
        let test_rocket = Server::start(rocket::custom(config)).expect("test rocket failed");
        Client::new(test_rocket).expect("test rocket launch failed")
    }

    fn device_id() -> String {
        String::from_utf8(
            thread_rng()
                .sample_iter(&distributions::Alphanumeric)
                .take(8)
                .collect(),
        )
        .unwrap()
    }

    fn user_id() -> String {
        format!("test-{}", device_id())
    }

    fn default_config_data() -> Table {
        let mut test_data = Table::new();
        let mut fxa_response = Table::new();
        fxa_response.insert("user".into(), "test".into());
        fxa_response.insert("client_id".into(), "test".into());
        fxa_response.insert(
            "scope".into(),
            vec![format!("{}send/bar", FxAAuthenticator::fxa_root("pushbox"))].into(),
        );
        test_data.insert("fxa_response".into(), fxa_response.into());
        test_data
    }

    #[test]
    fn test_valid_write() {
        let test_data = default_config_data();
        println!("test_data: {:?}", &test_data);
        let config = rocket_config(test_data);
        let client = rocket_client(config);
        let user_id = user_id();
        let url = format!("/v1/store/{}/{}", user_id, device_id());
        println!("URL: {}", url);
        let mut result = client
            .post(url.clone())
            .header(Header::new("Authorization", "bearer token"))
            .header(Header::new("Content-Type", "application/json"))
            .header(Header::new("X-Request-Id", "foobar123"))
            .body(r#"{"ttl": 60, "data":"Some Data"}"#)
            .dispatch();
        let body = &result.body_string().unwrap();
        assert!(result.status() == rocket::http::Status::raw(200));
        assert!(body.contains(r#""index":"#));
        assert!(body.contains(r#""status":200"#));

        // cleanup
        client
            .delete(url)
            .header(Header::new("Authorization", "bearer token"))
            .header(Header::new("Content-Type", "application/json"))
            .header(Header::new("FxA-Request-Id", "foobar123"))
            .dispatch();
    }

    #[test]
    fn test_valid_read() {
        let config = rocket_config(default_config_data());
        let client = rocket_client(config);
        let url = format!("/v1/store/{}/{}", user_id(), device_id());
        let mut write_result = client
            .post(url.clone())
            .header(Header::new("Authorization", "bearer token"))
            .header(Header::new("Content-Type", "application/json"))
            .header(Header::new("FxA-Request-Id", "foobar123"))
            .body(r#"{"ttl": 60, "data":"Some Data"}"#)
            .dispatch();
        let write_json: WriteResp = serde_json::from_str(
            &write_result
                .body_string()
                .expect("Empty body string for write"),
        )
        .expect("Could not parse write response body");
        let mut read_result = client
            .get(url.clone())
            .header(Header::new("Authorization", "bearer token"))
            .header(Header::new("Content-Type", "application/json"))
            .header(Header::new("FxA-Request-Id", "foobar123"))
            .dispatch();
        assert!(read_result.status() == rocket::http::Status::raw(200));
        let mut read_json: ReadResp = serde_json::from_str(
            &read_result
                .body_string()
                .expect("Empty body for read response"),
        )
        .expect("Could not parse read response");

        assert!(read_json.status == 200);
        assert!(!read_json.messages.is_empty());
        // a MySql race condition can cause this to fail.
        assert!(write_json.index <= read_json.index);
        // return the message at index
        read_result = client
            .get(format!("{}?index={}&limit=1", url, write_json.index))
            .header(Header::new("Authorization", "bearer token"))
            .header(Header::new("Content-Type", "application/json"))
            .header(Header::new("FxA-Request-Id", "foobar123"))
            .dispatch();

        read_json = serde_json::from_str(
            &read_result
                .body_string()
                .expect("Empty body for read query"),
        )
        .expect("Could not parse read query body");
        assert!(read_json.status == 200);
        assert!(read_json.messages.len() == 1);
        // a MySql race condition can cause these to fail.
        assert!(read_json.index == write_json.index);
        assert!(read_json.messages[0].index == write_json.index);

        // no data, no panic
        let empty_url = format!("/v1/store/{}/{}", user_id(), device_id());
        read_result = client
            .get(empty_url)
            .header(Header::new("Authorization", "bearer token"))
            .header(Header::new("Content-Type", "application/json"))
            .header(Header::new("FxA-Request-Id", "foobar123"))
            .dispatch();
        read_json = serde_json::from_str(
            &read_result
                .body_string()
                .expect("Empty body for read query"),
        )
        .expect("Could not parse read query body");
        assert!(read_json.status == 200);
        assert!(read_json.messages.is_empty());

        // cleanup
        client
            .delete(url)
            .header(Header::new("Authorization", "bearer token"))
            .header(Header::new("Content-Type", "application/json"))
            .header(Header::new("FxA-Request-Id", "foobar123"))
            .dispatch();
    }

    #[test]
    fn test_invalid_read_params() {
        let config = rocket_config(default_config_data());
        let client = rocket_client(config);
        let url = format!("/v1/store/{}/{}", user_id(), device_id());

        let mut read_result = client
            .get(format!("{}?limit=foo%20foo", url))
            .header(Header::new("Authorization", "bearer token"))
            .header(Header::new("Content-Type", "application/json"))
            .header(Header::new("FxA-Request-Id", "foobar123"))
            .dispatch();
        assert_eq!(read_result.status(), rocket::http::Status::BadRequest);
        let body: Value = serde_json::from_str(
            &read_result
                .body_string()
                .expect("Empty body for read query"),
        )
        .expect("Could not parse read query body");
        assert_eq!(body["errno"], 403);
        assert!(body["error"].as_str().unwrap().contains("foo%20foo"));
    }

    #[test]
    fn test_valid_delete() {
        let config = rocket_config(default_config_data());
        let client = rocket_client(config);
        let user_id = user_id();
        let url = format!("/v1/store/{}/{}", user_id, device_id());
        client
            .post(url.clone())
            .header(Header::new("Authorization", "bearer token"))
            .header(Header::new("Content-Type", "application/json"))
            .header(Header::new("FxA-Request-Id", "foobar123"))
            .body(r#"{"ttl": 60, "data":"Some Data"}"#)
            .dispatch();
        let mut del_result = client
            .delete(url.clone())
            .header(Header::new("Authorization", "bearer token"))
            .header(Header::new("Content-Type", "application/json"))
            .header(Header::new("FxA-Request-Id", "foobar123"))
            .dispatch();
        assert!(del_result.status() == rocket::http::Status::raw(200));
        let mut res_str = del_result.body_string().expect("Empty delete body string");
        assert!(res_str == "{}");
        let mut read_result = client
            .get(url.clone())
            .header(Header::new("Authorization", "bearer token"))
            .header(Header::new("Content-Type", "application/json"))
            .header(Header::new("FxA-Request-Id", "foobar123"))
            .dispatch();
        assert!(read_result.status() == rocket::http::Status::raw(200));
        res_str = read_result.body_string().expect("Empty read body string");
        let mut read_json: ReadResp =
            serde_json::from_str(&res_str).expect("Could not parse ready body");
        assert!(read_json.messages.is_empty());

        let read_result = client
            .delete(format!("/v1/store/{}", user_id))
            .header(Header::new("Authorization", "bearer token"))
            .header(Header::new("Content-Type", "application/json"))
            .header(Header::new("FxA-Request-Id", "foobar123"))
            .dispatch();
        assert!(read_result.status() == rocket::http::Status::raw(200));
        assert!(del_result.body_string() == None);

        let mut read_result = client
            .get(url)
            .header(Header::new("Authorization", "bearer token"))
            .header(Header::new("Content-Type", "application/json"))
            .header(Header::new("FxA-Request-Id", "foobar123"))
            .dispatch();
        assert!(read_result.status() == rocket::http::Status::raw(200));
        read_json = serde_json::from_str(
            &read_result
                .body_string()
                .expect("Empty verification body string"),
        )
        .expect("Could not parse verification body string");
        assert!(read_json.messages.is_empty());
    }

    #[test]
    fn test_version() {
        let config = rocket_config(default_config_data());
        let client = rocket_client(config);
        let mut response = client.get("/__version__").dispatch();
        assert_eq!(response.status(), rocket::http::Status::Ok);
        let result: JsonValue =
            serde_json::from_str(&response.body_string().expect("Empty body for read query"))
                .expect("Could not parse read query body");
        assert_eq!(result["version"], "TBD");
    }

    #[test]
    fn test_heartbeat() {
        let config = rocket_config(default_config_data());
        let client = rocket_client(config);
        let mut response = client.get("/__heartbeat__").dispatch();
        assert_eq!(response.status(), rocket::http::Status::Ok);
        let result: JsonValue =
            serde_json::from_str(&response.body_string().expect("Empty body for read query"))
                .expect("Could not parse read query body");
        assert_eq!(result["code"], 200);
        assert_eq!(result["fxa_auth"], "oauth.stage.mozaws.net");
        assert_eq!(result["database"], "ok");
    }

    #[test]
    fn test_lbheartbeat() {
        let config = rocket_config(default_config_data());
        let client = rocket_client(config);
        let mut response = client.get("/__lbheartbeat__").dispatch();
        assert_eq!(response.status(), rocket::http::Status::Ok);
        assert!(response.body().is_none());
    }
}
