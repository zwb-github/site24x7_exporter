use anyhow::{anyhow, Context, Result};
use http::uri::PathAndQuery;
use hyper::service::{make_service_fn, service_fn};
use hyper::{header, Body, Method, Request, Response, Server, StatusCode};
use lazy_static::lazy_static;
use log::{debug, error, info};
use prometheus::{Encoder, GaugeVec, IntGaugeVec, TextEncoder, Registry};
use simplelog::{LevelFilter, TermLogger};
use std::net::SocketAddr;
use structopt::StructOpt;

mod site24x7_types;
mod zoho_types;

lazy_static! {
    static ref CLIENT: reqwest::Client = reqwest::Client::new();
}

#[derive(StructOpt, Clone, Debug)]
#[structopt(
    name = "site24x7_exporter",
    author,
    about,
    global_settings = &[structopt::clap::AppSettings::ColoredHelp],
)]
pub struct Config {
    /// API endpoint to use (depends on region, see https://site24x7.com/help/api)
    #[structopt(long, default_value = "site24x7.com",
        possible_values = &["site24x7.com", "site24x7.eu", "site24x7.cn", "site24x7.in", "site24x7.net.au"])]
    pub site24x7_endpoint: String,

    /// Address on which to expose metrics and web interface
    #[structopt(long = "web.listen-address", default_value = "0.0.0.0:9803")]
    pub listen_address: SocketAddr,

    /// Path under which to expose metrics
    #[structopt(long = "web.telemetry-path", default_value = "/metrics")]
    pub metrics_path: PathAndQuery,

    /// Only log messages with the given severity or above
    #[structopt(
        long = "log.level",
        default_value = "info",
        possible_values = &["error", "warn", "info", "debug", "trace"],
    )]
    pub loglevel: LevelFilter,
}

/// Acquire the access token.
///
/// An access token is a short-lived token that can be used to query the
/// API multiple times. It will become invalidated after a short period of
/// time.
/// See https://www.site24x7.com/help/api/index.html#authentication
async fn get_access_token(
    client: &reqwest::Client,
    site24x7_client_info: &site24x7_types::Site24x7ClientInfo,
    refresh_token: &str,
) -> Result<String> {
    let access_token_request = zoho_types::AccessTokenRequest {
        client_id: site24x7_client_info.client_id.clone(),
        client_secret: site24x7_client_info.client_secret.clone(),
        refresh_token: refresh_token.into(),
        grant_type: "refresh_token".into(),
    };

    let access_token_endpoint = format!("{}/oauth/v2/token", &site24x7_client_info.zoho_endpoint);
    info!("Requesting access token from {}", access_token_endpoint);
    debug!(
        "Getting access token with info:\n{:#?}",
        access_token_request
    );
    let access_token_resp = client
        .post(&access_token_endpoint)
        .form(&access_token_request)
        .send()
        .await?;

    let access_token_resp_text = access_token_resp.text().await?;

    let access_token_resp_parsed =
        serde_json::from_str(&access_token_resp_text).context(format!(
            "Couldn't parse server response while getting access token. Server replied: '{}",
            access_token_resp_text
        ))?;
    match access_token_resp_parsed {
        zoho_types::AccessTokenResponse::Success(inner) => {
            info!("Successfully acquired access token");
            debug!("Access token value: {}", inner.access_token);
            Ok(inner.access_token)
        }
        zoho_types::AccessTokenResponse::Error(e) => Err(anyhow!(
            "Error while getting access token. Server replied '{}'",
            e.error
        )),
    }
}

/// Receive an update for all monitor statuses.
///
/// Given a valid `access_token`, this will try to get a new set of fresh monitor data.
async fn fetch_current_status(
    client: &reqwest::Client,
    site24x7_endpoint: &str,
    access_token: &str,
) -> Result<site24x7_types::CurrentStatusData, site24x7_types::CurrentStatusError> {
    let current_status_resp = client
        .get(&format!("{}/current_status", site24x7_endpoint))
        .header("Accept", "application/json; version=2.0")
        .header("Authorization", format!("Zoho-oauthtoken {}", access_token))
        .send()
        .await
        .context("Error during web request to fetch curent status.")?;

    let current_status_resp_text = current_status_resp
        .text()
        .await
        .context("Couldn't stream text from response")?;

    let deserializer = &mut serde_json::Deserializer::from_str(&current_status_resp_text);
    let current_status_resp_result = serde_path_to_error::deserialize(deserializer);

    let v: serde_json::Value =
        serde_json::from_str(&current_status_resp_text).context("JSON seems invalid.")?;
    debug!(
        "JSON received from server: \n{}",
        serde_json::to_string_pretty(&v).context("Couldn't format JSON for debug output")?
    );
    let current_status_resp_parsed: site24x7_types::CurrentStatusResponse =
        current_status_resp_result
            .map_err(|e| {
                // For better error path output, try to parse into `CurrentStatusResponseInner`
                // directly. This will give us a path to the error.
                let debug_deserializer =
                    &mut serde_json::Deserializer::from_str(&current_status_resp_text);
                let debug_deserializer_result: Result<
                    site24x7_types::CurrentStatusResponseInner,
                    _,
                > = serde_path_to_error::deserialize(debug_deserializer);
                let debug_err = debug_deserializer_result.err();
                anyhow!(site24x7_types::CurrentStatusError::ParseError(
                    e.to_string()
                ))
                .context(debug_err.map(|e| e.to_string()).unwrap_or_default())
            })
            .context("Couldn't parse server response while fetching monitors.".to_string())?;

    match current_status_resp_parsed {
        site24x7_types::CurrentStatusResponse::Success(inner) => Ok(inner.data),
        site24x7_types::CurrentStatusResponse::Error(e) => {
            if e.message == "OAuth Access Token is invalid or has expired." {
                Err(site24x7_types::CurrentStatusError::ApiAuthError(e.message))
            } else {
                Err(site24x7_types::CurrentStatusError::ApiUnknownError(
                    e.message,
                ))
            }
        }
    }
}

/// Set the Prometheus metrics for a specfic monitor.
fn set_metrics(monitors: &[site24x7_types::MonitorMaybe], monitor_group: &str) {
    for monitor_maybe in monitors {
        let monitor_type = monitor_maybe.to_string();
        let monitor = match monitor_maybe {
            site24x7_types::MonitorMaybe::URL(m)
            | site24x7_types::MonitorMaybe::HOMEPAGE(m)
            | site24x7_types::MonitorMaybe::REALBROWSER(m) => m,
            _ => continue,
        };
        for location in &monitor.locations {
            debug!(
                "Setting MONITOR_UP_GAUGE with {{monitor_type=\"{}\", \
                        monitor_name=\"{}\", monitor_group=\"{}\", location=\"{}\"}} \
                        to {}",
                &monitor_type,
                &monitor.name,
                &monitor_group,
                &location.location_name,
                location.clone().status as i64
            );
            MONITOR_UP_GAUGE
                .with_label_values(&[
                    &monitor_type,
                    &monitor.name,
                    &monitor_group,
                    &location.location_name,
                ])
                .set(location.clone().status as i64);

            // The original gauge is in milliseconds. Convert it to seconds first as prometheus wants
            // its time series data in seconds.
            MONITOR_LATENCY_SECONDS_GAUGE
                .with_label_values(&[
                    &monitor_type,
                    &monitor.name,
                    &monitor_group,
                    &location.location_name,
                ])
                .set(location.clone().attribute_value as f64 / 1000.0);
        }
    }
}

async fn hyper_service(
    req: Request<Body>,
    site24x7_client_info: &site24x7_types::Site24x7ClientInfo,
    refresh_token: &str,
    access_token: &str,
    metrics_path: &str,
) -> Result<Response<Body>, hyper::error::Error> {
    let mut access_token = access_token.to_owned();

    if req.method() != Method::GET || req.uri().path() != metrics_path {
        return Ok(Response::new(
            format!("site24x7_exporter\n\nTry {}", metrics_path).into(),
        ));
    }

    let current_status = fetch_current_status(
        &CLIENT,
        &site24x7_client_info.site24x7_endpoint,
        &access_token,
    )
    .await;

    let current_status_data = match current_status {
        Ok(ref current_status_data) => {
            debug!(
                "Successfully deserialized into this data structure: \n{:#?}",
                &current_status
            );
            current_status_data.clone()
        }
        // If there was an auth error, maybe the token was old. We'll try to get a new token.
        // If we also get an auth error the second time, probably something is wrong with the
        // refresh token and we'll just give up.
        Err(site24x7_types::CurrentStatusError::ApiAuthError(_)) => {
            info!(
                "Couldn't get status update due to an authentication error. \
                Probably the access token has timed out. Trying to get a new one."
            );
            let access_token_res =
                get_access_token(&CLIENT, &site24x7_client_info, &refresh_token).await;
            access_token = match access_token_res {
                Ok(access_token) => access_token,
                Err(e) => {
                    error!("{:?}", e);
                    return Ok(Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::from(e.to_string()))
                        .unwrap());
                }
            };

            match fetch_current_status(
                &CLIENT,
                &site24x7_client_info.site24x7_endpoint,
                &access_token,
            )
            .await
            {
                Ok(current_status_data) => current_status_data,
                Err(e) => {
                    error!("{:?}", e);
                    return Ok(Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::from(e.to_string()))
                        .unwrap());
                }
            }
        }
        Err(e) => {
            error!("{:?}", e);
            return Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from(e.to_string()))
                .unwrap());
        }
    };

    // Update metrics based on the API data gathered above.

    let registry = Registry::new();
    MONITOR_UP_GAUGE.reset();
    MONITOR_LATENCY_SECONDS_GAUGE.reset();
    static ref MONITOR_UP_GAUGE: IntGaugeVec = prometheus::register_int_gauge_vec!(
        "site24x7_monitor_up",
        "Current health status of the monitor (1 = UP, 0 = DOWN).",
        &["monitor_type", "monitor_name", "monitor_group", "location"]
    )
    .expect("Couldn't create monitor_up metric");
    static ref MONITOR_LATENCY_SECONDS_GAUGE: GaugeVec = prometheus::register_gauge_vec!(
        "site24x7_monitor_latency_seconds",
        "Last measured latency in seconds.",
        &["monitor_type", "monitor_name", "monitor_group", "location"]
    )
    .expect("Couldn't create monitor_latency_seconds metric");

    // Monitors can either be in a flat list of plain Monitors or they can be inside of a
    // MonitorGroup with is simply a list of monitors.
    set_metrics(&current_status_data.monitors, "");

    for monitor_group in current_status_data.monitor_groups {
        set_metrics(&monitor_group.monitors, &monitor_group.group_name);
    }

    let metric_families = prometheus::gather();
    let mut buffer = vec![];
    let encoder = TextEncoder::new();
    encoder.encode(&metric_families, &mut buffer).unwrap();
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_ENCODING, encoder.format_type())
        .body(Body::from(buffer))
        .unwrap())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Config::from_args();

    TermLogger::init(
        args.loglevel,
        simplelog::ConfigBuilder::new()
            .set_thread_level(simplelog::LevelFilter::Trace)
            .build(),
        simplelog::TerminalMode::Mixed,
    )?;

    dotenv::dotenv().ok();

    let client_id = std::env::var("ZOHO_CLIENT_ID").context("ZOHO_CLIENT_ID must be set")?;
    let client_secret =
        std::env::var("ZOHO_CLIENT_SECRET").context("ZOHO_CLIENT_SECRET must be set")?;
    let refresh_token =
        std::env::var("ZOHO_REFRESH_TOKEN").context("ZOHO_REFRESH_TOKEN must be set")?;

    let site24x7_client_info = site24x7_types::Site24x7ClientInfo {
        site24x7_endpoint: format!("https://{}/api", args.site24x7_endpoint),
        zoho_endpoint: format!(
            "https://accounts.zoho.{}",
            args.site24x7_endpoint.splitn(2, '.').last().unwrap()
        ),
        client_id,
        client_secret,
    };

    // Figure out Zoho accounts endpoint.
    info!(
        "Using site24x7 endpoint: {}",
        site24x7_client_info.site24x7_endpoint
    );
    info!(
        "Using Zoho endpoint: {}",
        site24x7_client_info.zoho_endpoint
    );

    // Info print used proxies if there are any.
    // Currently we have to do this in a stupid backwards way by parsing the debug output.
    // Hopefully, we'll be able to do this properly once this is fixed:
    // https://github.com/seanmonstar/reqwest/issues/967
    let debug_output = format!("{:?}", *CLIENT);
    let re = regex::Regex::new(r"^.*System\(\{(.*?)\}").unwrap();
    if let Some(caps) = re.captures(&debug_output) {
        if let Some(cap) = caps.get(1) {
            if cap.as_str().is_empty() {
                info!("Not using any proxies");
            } else {
                info!("Picked up proxies: {}", &caps[1]);
            }
        }
    }

    debug!("Reqwest client:\n{:#?}", *CLIENT);

    // An access token is only available for a period of time.
    // We sometimes have to refresh it.
    let access_token = get_access_token(&CLIENT, &site24x7_client_info, &refresh_token).await?;

    let metrics_path = args.metrics_path.to_string();
    let make_service = make_service_fn(move |_conn| {
        let site24x7_client_info = site24x7_client_info.clone();
        let refresh_token = refresh_token.clone();
        let access_token = access_token.clone();
        let metrics_path = metrics_path.clone();
        async move {
            Ok::<_, hyper::Error>(service_fn(move |req| {
                let site24x7_client_info = site24x7_client_info.clone();
                let refresh_token = refresh_token.clone();
                let access_token = access_token.clone();
                let metrics_path = metrics_path.clone();
                async move {
                    hyper_service(
                        req,
                        &site24x7_client_info,
                        &refresh_token,
                        &access_token,
                        &metrics_path,
                    )
                    .await
                }
            }))
        }
    });

    let server = Server::bind(&args.listen_address).serve(make_service);

    server.await.context("Server error")
}
