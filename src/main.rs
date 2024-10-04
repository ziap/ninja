use std::{cmp, process};
use std::path::{Path, PathBuf};
use std::net::{IpAddr, SocketAddr};

use axum::{extract, http, response, routing, Router};
use tokio::{fs, process::Command};
use tokio::io::{AsyncReadExt, AsyncSeekExt, self};

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Config {
    video_path: Box<Path>,
    ip: IpAddr,
    port: u16,
    chunk_size: u64,
    ffmpeg_command: Box<str>
}

impl Default for Config {
    fn default() -> Self {
        Config {
            video_path: Path::new("videos/").into(),
            ip: [0, 0, 0, 0].into(),
            port: 3000,
            chunk_size: 65536,
            ffmpeg_command: "ffmpeg".into()
        }
    }
}

#[derive(serde::Deserialize)]
struct FrameQuery {
    t: u32
}

#[tokio::main]
async fn main() {
    const CONFIG_PATH: &str = "config.toml";
    let config = if let Ok(mut file) = fs::File::open(CONFIG_PATH).await {
        let mut config_str = String::new();
        if let Err(err) = file.read_to_string(&mut config_str).await {
            eprintln!("ERROR: Failed to read configuration: {err}");
            process::exit(1);
        }

        match toml::from_str(&config_str) {
            Ok(config) => config,
            Err(err) => {
                eprintln!("ERROR: Failed to parse configuration: {err}");
                process::exit(1);
            }
        }
    } else {
        let default_config = Config::default();
        let config_str = toml::to_string_pretty(&default_config).unwrap();

        if let Err(err) = fs::write(CONFIG_PATH, config_str).await {
            eprintln!("ERROR: Failed to write default configuration: {err}");
        }

        default_config
    };

    let config_ref = Box::leak(config.into());

    let app = Router::new()
        .route("/video/:video", routing::get(serve_video))
        .route("/frame/:video", routing::get(serve_frame))
        .with_state(config_ref);

    let addr = SocketAddr::from((config_ref.ip, config_ref.port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("ERROR: Failed to bind socket: {err}");
            process::exit(1);
        }
    };
    println!("Server listening on {addr}");
    if let Err(err) = axum::serve(listener, app).await {
        eprintln!("ERROR: Failed to start server: {err}");
        process::exit(1);
    }
}

async fn serve_video(
    extract::Path((video, )): extract::Path<(Box<str>, )>,
    header: http::HeaderMap,
    extract::State(config): extract::State<&Config>
) -> response::Response {
    let video_path: PathBuf = config.video_path.join(&*video);

    let mut video = match fs::File::open(&video_path).await {
        Ok(video) => video,
        Err(err) => {
            eprintln!("ERROR: Failed to open video `{}`: {err}", video_path.display());
            return response::Response::builder()
                .status(http::StatusCode::NOT_FOUND)
                .body("Video not found".into())
                .unwrap();
        }
    };

    let size = video.seek(io::SeekFrom::End(0)).await.unwrap();

    let (start, end) = if let Some(header_str) = header.get(http::header::RANGE) {
        let header_str = header_str.to_str().unwrap_or("");
        let range = if &header_str[..6] == "bytes=" { &header_str[6..] } else { "" };

        if &range[..1] == "-" {
            let last: u64 = range[1..].parse().unwrap_or(0);

            (size - last, size - 1)
        } else {
            let (start_str, end_str) = range.split_once('-').unwrap_or(("", ""));
            let start: u64 = start_str.parse().unwrap_or(0);
            let end: u64 = end_str.parse().unwrap_or(cmp::min(start + config.chunk_size, size) - 1);
            (start, end)
        }
    } else {
        let mut buffer = vec![0; size as usize];

        video.seek(io::SeekFrom::Start(0)).await.unwrap();
        video.read_exact(&mut buffer).await.unwrap();

        return response::Response::builder()
            .status(http::StatusCode::OK)
            .header(http::header::ACCEPT_RANGES, "bytes")
            .body(buffer.into())
            .unwrap()
    };

    if end >= size {
        return response::Response::builder()
            .status(http::StatusCode::RANGE_NOT_SATISFIABLE)
            .body("Range Not Satisfiable".into())
            .unwrap();
    }

    let range_size = end + 1 - start;
    let mut buffer = vec![0; range_size as usize];
    video.seek(io::SeekFrom::Start(start)).await.unwrap();
    video.read_exact(&mut buffer).await.unwrap();

    response::Response::builder()
        .status(http::StatusCode::PARTIAL_CONTENT)
        .header(http::header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"))
        .header(http::header::ACCEPT_RANGES, "bytes")
        .header(http::header::CONTENT_TYPE, "video/mp4")
        .body(buffer.into())
        .unwrap()
}

async fn serve_frame(
    extract::Path((video, )): extract::Path<(Box<Path>, )>,
    extract::Query(params): extract::Query<FrameQuery>,
    extract::State(config): extract::State<&Config>
) -> response::Response {
    let video_path: PathBuf = [&*config.video_path, &*video].iter().collect();
    let t = params.t;
    if let Ok(true) = fs::try_exists(&video_path).await {
        let stdout = match Command::new(&*config.ffmpeg_command).args([
            "-ss", &t.to_string(),
            "-i", video_path.to_str().unwrap(),
            "-vframes", "1",
            "-f", "image2pipe",
            "-vcodec", "mjpeg",
            "-"
        ]).output().await {
            Ok(output) => output.stdout,
            Err(err) => {
                eprintln!("ERROR: Failed to extract frame: {err}");
                return response::Response::builder()
                    .status(http::StatusCode::INTERNAL_SERVER_ERROR)
                    .body("Failed to extract frame".into())
                    .unwrap()
            }
        };

        response::Response::builder()
            .status(http::StatusCode::OK)
            .header(http::header::CONTENT_TYPE, "image/jpeg")
            .body(stdout.into())
            .unwrap()
    } else {
        response::Response::builder()
            .status(http::StatusCode::NOT_FOUND)
            .body("Video not found".into()).unwrap()
    }
}
