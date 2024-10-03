use std::fmt::Write;
use std::num::NonZeroU64;
use std::process::ExitCode;
use std::{
    ffi::OsString,
    num::NonZeroUsize,
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use clap::Parser;
use futures_util::StreamExt;
use parking_lot::Mutex;
use quantiles::ckms::CKMS;
use reqwest::Response;
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Method, Url,
};
use spdlog::{debug, error, info, Level, LevelFilter};

#[derive(clap::Parser, Debug)]
struct Args {
    count: NonZeroU64,
    url: Url,

    #[arg(short = 'X', long = "request", default_value_t = Method::GET)]
    method: Method,

    #[arg(short = 'H', long = "header")]
    header: Vec<String>,

    #[arg(short = 'T', long, value_parser = humantime::parse_duration, default_value = "5s", help = "Timeout for each request")]
    timeout: Duration,

    #[arg(short = 'd', long)]
    data: Option<OsString>,

    #[arg(
        short = 'P',
        long,
        help = "Number of tasks to spawn (defaults to ncpus * 4)"
    )]
    tasks: Option<NonZeroUsize>,

    #[arg(
        short = 's',
        long,
        help = "Suppress output of requests, including errors which then will only be printed at the end"
    )]
    silent: bool,
}

fn main() -> ExitCode {
    tokio::runtime::Builder::new_multi_thread()
        .enable_io()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async { start() })
}

fn start() -> ExitCode {
    let args = Args::parse();

    let default_logger = spdlog::default_logger();

    let Ok(level) =
        std::env::var("LOGLVL").map_or(Ok(Level::Info), |s| Level::from_str(s.as_str()))
    else {
        error!("Invalid value for $LOGLVL");
        return ExitCode::FAILURE;
    };

    default_logger.set_level_filter(LevelFilter::MoreSevereEqual(level));

    debug!("Args: {args:?}");

    let client = reqwest::Client::builder()
        .timeout(args.timeout)
        .build()
        .unwrap();

    let mut headers = HeaderMap::with_capacity(args.header.len());
    let unparsed_headers = Box::leak(args.header.into_boxed_slice());
    for header in unparsed_headers {
        let Some((k, v)) = header.split_once(':') else {
            error!("Malformed header: {header:?}");
            return ExitCode::FAILURE;
        };

        headers.insert(k.trim(), HeaderValue::from_str(v.trim()).unwrap());
    }
    debug!("Headers: {headers:?}");

    let mut request = client.request(args.method, args.url).headers(headers);
    if let Some(content) = args.data {
        request = request.body(content.into_encoded_bytes());
    }

    let request = match request.build() {
        Ok(r) => r,
        Err(e) => {
            error!("Could not build request: {e:?}");
            return ExitCode::FAILURE;
        }
    };

    let tasks = args.tasks.map_or_else(
        || {
            std::thread::available_parallelism()
                .map(NonZeroUsize::get)
                .unwrap_or(1)
                * 4
        },
        NonZeroUsize::get,
    );

    let idx = AtomicU64::from(1);
    let err_msg = Mutex::new(String::new());
    let start = std::time::Instant::now();

    let percentiles = Mutex::new(CKMS::<f64>::new(0.001));
    let mean = AtomicU64::from(f64::to_bits(0.0));

    let count = args.count.get();
    async_scoped::TokioScope::scope_and_block(|s| {
        for _ in 0..tasks {
            let client = &client;
            let request = &request;
            let idx = &idx;
            let percentiles = &percentiles;
            let mean = &mean;
            let err_msg = &err_msg;

            s.spawn(async move {
                let mut buf = Vec::new();

                loop {
                    let idx = idx.fetch_add(1, Ordering::Relaxed);
                    if idx > count {
                        break;
                    }

                    let start = std::time::Instant::now();
                    match client
                        .execute(request.try_clone().unwrap())
                        .await
                        .and_then(Response::error_for_status)
                    {
                        Ok(res) => {
                            let elapsed = start.elapsed();
                            percentiles
                                .lock()
                                .insert(elapsed.as_micros() as f64 / 1000.);
                            _ = mean.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                                Some(f64::to_bits(
                                    f64::from_bits(n) + elapsed.as_micros() as f64 / count as f64,
                                ))
                            });

                            if args.silent {
                                continue;
                            }

                            let status = res.status();

                            let log_empty = || {
                                info!(
                                    "[{}/{}] [{}] in {:.02}ms",
                                    idx,
                                    count,
                                    status,
                                    elapsed.as_micros() as f64 / 1000.,
                                );
                            };

                            if res.content_length().is_some_and(|n| n == 0) {
                                log_empty();
                                continue;
                            }

                            buf.clear();
                            let mut stream = res.bytes_stream();
                            while let Some(Ok(b)) = stream.next().await {
                                buf.extend_from_slice(&b);
                            }

                            if buf.is_empty() {
                                log_empty();
                                continue;
                            }

                            let str = String::from_utf8_lossy(&buf);
                            info!(
                                "[{}/{}] [{}] in {:.02}ms: {}",
                                idx,
                                count,
                                status,
                                elapsed.as_micros() as f64 / 1000.,
                                str
                            );
                        }
                        Err(e) => {
                            let elapsed = start.elapsed();
                            if !args.silent {
                                if let Some(status) = e.status() {
                                    error!(
                                        "[{}/{}] [{}] in {:.02}ms: {:?}",
                                        idx,
                                        count,
                                        status,
                                        elapsed.as_micros() as f64 / 1000.,
                                        e
                                    );
                                } else {
                                    error!(
                                        "[{}/{}] [N/A] in {:.02}ms: {:?}",
                                        idx,
                                        count,
                                        elapsed.as_micros() as f64 / 1000.,
                                        e
                                    );
                                }
                            }

                            let mut err_msg = err_msg.lock();
                            if err_msg.is_empty() {
                                _ = writeln!(err_msg, "Errors:\n- {e}");
                            } else {
                                _ = writeln!(err_msg, "- {e}");
                            }
                        }
                    };
                }
            });
        }
    });

    let elapsed = start.elapsed();

    let mut exit = ExitCode::SUCCESS;
    let err_msg = err_msg.into_inner();
    if !err_msg.is_empty() {
        error!("{}", &err_msg[..err_msg.len() - 1]);
        exit = ExitCode::FAILURE;
    }

    let percentiles = percentiles.into_inner();

    info!(
        "Sent {} requests in {:.04}s ({:.02} rps / {:.02}ms mean)\n- Stats: [ p0 (min): {:.02}ms / p1: {:.02}ms / p25: {:.02}ms / p50 (median): {:.02}ms / p75: {:.02}ms / p99: {:.02}ms / p100 (max): {:.02}ms ]",
        count,
        elapsed.as_millis() as f64 / 1000.,
        (count as f64 / elapsed.as_secs_f64()),
        f64::from_bits(mean.load(Ordering::Relaxed)) / 1000.,
        percentiles.query(0.00).unwrap_or((0, f64::NAN)).1,
        percentiles.query(0.01).unwrap_or((0, f64::NAN)).1,
        percentiles.query(0.25).unwrap_or((0, f64::NAN)).1,
        percentiles.query(0.50).unwrap_or((0, f64::NAN)).1,
        percentiles.query(0.75).unwrap_or((0, f64::NAN)).1,
        percentiles.query(0.99).unwrap_or((0, f64::NAN)).1,
        percentiles.query(1.00).unwrap_or((0, f64::NAN)).1,
    );

    exit
}
