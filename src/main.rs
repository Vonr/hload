use std::fmt::Write;
use std::num::NonZeroU64;
use std::process::ExitCode;
use std::{
    ffi::OsString,
    num::NonZeroUsize,
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use clap::Parser;
use quantiles::ckms::CKMS;
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Method, Url,
};
use spdlog::{log, Level, LevelFilter};

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
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    let default_logger = spdlog::default_logger();
    default_logger.set_level_filter(LevelFilter::MoreSevereEqual(
        std::env::var("LOGLVL")
            .map(|s| Level::from_str(s.as_str()).unwrap_or(Level::Info))
            .unwrap_or(Level::Info),
    ));

    log!(Level::Debug, "Args: {:?}", args);

    let client = reqwest::Client::builder()
        .timeout(args.timeout)
        .build()
        .unwrap();

    let mut headers = HeaderMap::with_capacity(args.header.len());
    let unparsed_headers = Box::leak(args.header.into_boxed_slice());
    for header in unparsed_headers {
        let Some((k, v)) = header.split_once(':') else {
            log!(Level::Error, "Malformed header {:?}", header);
            std::process::exit(1);
        };

        headers.insert(k.trim(), HeaderValue::from_str(v.trim()).unwrap());
    }
    log!(Level::Debug, "Headers: {:?}", headers);

    let mut request = client.request(args.method, args.url).headers(headers);
    if let Some(content) = args.data {
        request = request.body(content.into_encoded_bytes());
    }

    let request = match request.build() {
        Ok(r) => r,
        Err(e) => {
            log!(Level::Error, "Could not build request: {e:?}");
            std::process::exit(1);
        }
    };

    let tasks = args.tasks.map(NonZeroUsize::get).unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(NonZeroUsize::get)
            .unwrap_or(1)
            * 4
    });

    let idx = Arc::new(AtomicU64::from(1));
    let mut futures = Vec::with_capacity(tasks);
    let (errs_send, errs_recv) = flume::unbounded();
    let count = args.count.get();
    let (timings_send, timings_recv) = flume::bounded(count as usize);
    let start = std::time::Instant::now();

    for _ in 0..tasks {
        let client = client.clone();
        let request = request.try_clone().unwrap();
        let idx = idx.clone();
        let errs_send = errs_send.clone();
        let timings_send = timings_send.clone();

        futures.push(tokio::spawn(async move {
            loop {
                let idx = idx.fetch_add(1, Ordering::Relaxed);
                if idx > count {
                    break;
                }

                let start = std::time::Instant::now();
                match client
                    .execute(request.try_clone().unwrap())
                    .await
                    .and_then(|r| r.error_for_status())
                {
                    Ok(res) => {
                        let elapsed = start.elapsed();
                        timings_send.send_async(elapsed).await.unwrap();
                        log!(
                            Level::Info,
                            "[{}/{}] [{}] in {:.02}ms: {}",
                            idx,
                            args.count,
                            res.status(),
                            elapsed.as_micros() as f64 / 1000.,
                            res.text().await.unwrap_or_else(|_| "<unknown>".to_string())
                        )
                    }
                    Err(e) => {
                        log!(Level::Error, "{}", &e);
                        let _ = errs_send.send_async(e).await;
                    }
                };
            }
        }));
    }

    futures_buffered::join_all(futures).await;
    let elapsed = start.elapsed();

    let mut exit = ExitCode::SUCCESS;
    if !errs_recv.is_empty() {
        log!(
            Level::Error,
            "Errors:\n{}",
            errs_recv
                .drain()
                .fold(String::new(), |mut acc, e| {
                    writeln!(acc, "- {e}").unwrap();
                    acc
                })
                .trim_end_matches("\n")
        );
        exit = ExitCode::FAILURE;
    }

    let mut percentiles = CKMS::<f64>::new(0.001);
    let mut mean = 0.0;
    for timing in timings_recv.drain() {
        percentiles.insert(timing.as_micros() as f64 / 1000.);
        mean += timing.as_micros() as f64 / count as f64;
    }

    log!(
        Level::Info,
        "Sent {} requests in {:.04}s ({:.02} rps / {:.02}ms mean)\n- Stats: [ p0 (min): {:.02}ms / p1: {:.02}ms / p25: {:.02}ms / p50 (median): {:.02}ms / p75: {:.02}ms / p99: {:.02}ms / p100 (max): {:.02}ms ]",
        count,
        elapsed.as_millis() as f64 / 1000.,
        (count as f64 / elapsed.as_secs_f64()),
        mean / 1000.,
        percentiles.query(0.00).unwrap().1,
        percentiles.query(0.01).unwrap().1,
        percentiles.query(0.25).unwrap().1,
        percentiles.query(0.50).unwrap().1,
        percentiles.query(0.75).unwrap().1,
        percentiles.query(0.99).unwrap().1,
        percentiles.query(1.00).unwrap().1,
    );

    exit
}
