//! MOA 2026 제출 CLI.
//!
//! 참가자가 쓰는 명령은 넷이다.
//!
//!   moa-submitter login          GitHub 로 로그인하고 토큰을 저장한다
//!   moa-submitter submit         레포에서 커널 소스를 찾아 올린다
//!   moa-submitter status [ID]    제출 목록, 또는 한 제출의 자세한 결과
//!   moa-submitter log ID         그 제출의 로그를 단계별로 본다
//!
//! 이 도구는 제출물을 검증하지 않는다. contract 판정은 서버 한 곳에서만 내려야
//! 여기서 통과한 것이 거기서 떨어지는 일이 생기지 않는다.

// ureq::Error 는 응답을 통째로 물고 있어서 크다. 그 타입을 그대로 나르는 것이
// ureq 를 쓰는 방식이므로 박싱하지 않는다.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine;
use clap::{Parser, Subcommand};
use serde::Deserialize;

const DEFAULT_SERVER: &str = "https://micro2026-api.duckdns.org:7777";

/// 참가자가 올릴 수 있는 파일. 서버의 allowlist 와 같아야 한다.
/// ops_vision.rs 와 ops_audio.rs 는 채점하는 세 커널과 무관하므로 보내지 않는다.
const ALLOWED_FILE: &str = "src/ops.rs";
const ALLOWED_DIR: &str = "src/device";

/// 서버 상한과 같은 값. 여기서 먼저 걸러야 참가자가 왜 거부됐는지 바로 안다.
const MAX_FILES: usize = 1000;
const MAX_TOTAL_BYTES: u64 = 10 * 1024 * 1024;

/// 채점하는 커널. 결과를 테스트가 도는 순서로 보여주기 위한 것이다.
const KERNEL_ORDER: &[&str] = &[
    "ops::sliding_project_qkv",
    "ops::sliding_attention_output",
    "ops::decoder_feedforward",
];

const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

// 첫 연결이 튕기고 두 번째에 붙는 망이 있다. 전송 계층 실패만 다시 시도한다 --
// 서버가 상태 코드로 답한 것은 다시 물어도 같은 답이므로 재시도하지 않는다.
const CONNECT_ATTEMPTS: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// 종료 코드
// ---------------------------------------------------------------------------

/// 1 은 "요청은 닿았고 거부됐다", 3 은 "닿지 못했다". 스크립트가 재시도할지
/// 말지를 이 둘로 가른다.
const EXIT_REJECTED: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_SERVICE: i32 = 3;

struct Failure {
    message: String,
    code: i32,
}

impl Failure {
    fn rejected(message: impl Into<String>) -> Self {
        Self { message: message.into(), code: EXIT_REJECTED }
    }
    fn usage(message: impl Into<String>) -> Self {
        Self { message: message.into(), code: EXIT_USAGE }
    }
    fn service(message: impl Into<String>) -> Self {
        Self { message: message.into(), code: EXIT_SERVICE }
    }
}

type Result<T> = std::result::Result<T, Failure>;

// ---------------------------------------------------------------------------
// 명령
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "moa-submitter",
    version,
    about = "Submit kernels to the MOA 2026 kernel optimization competition.",
    after_help = "Run `moa-submitter login` once, then `moa-submitter submit` from inside your \
                  clone of the baseline repository."
)]
struct Cli {
    /// Competition server. Overrides $MOA_API_URL.
    #[arg(long, global = true, value_name = "URL")]
    server: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Log in with GitHub and store a submission token.
    Login,

    /// Upload your kernel sources and start a submission.
    Submit {
        /// Repository to submit. Defaults to the baseline clone containing the
        /// current directory.
        #[arg(long, value_name = "PATH")]
        source: Option<PathBuf>,
    },

    /// Show your submissions, or one submission in detail.
    Status {
        /// A submission id. Omit to list them all.
        #[arg(value_name = "ID")]
        submission: Option<String>,
    },

    /// Show the log of one submission.
    Log {
        #[arg(value_name = "ID")]
        submission: String,
    },
}

fn main() {
    let cli = Cli::parse();
    let server = match resolve_server(cli.server.as_deref()) {
        Ok(server) => server,
        Err(failure) => quit(failure),
    };

    let outcome = match cli.command {
        Command::Login => login(&server),
        Command::Submit { source } => submit(&server, source.as_deref()),
        Command::Status { submission } => status(&server, submission.as_deref()),
        Command::Log { submission } => show_log(&server, &submission),
    };

    if let Err(failure) = outcome {
        quit(failure);
    }
}

fn quit(failure: Failure) -> ! {
    eprintln!("{}", failure.message);
    std::process::exit(failure.code)
}

/// 끝의 `/` 와 경로를 떼어내 origin 만 남긴다. 참가자가 주소 끝에 슬래시를
/// 붙여도 `//api/...` 가 되지 않게 한다.
fn resolve_server(flag: Option<&str>) -> Result<String> {
    let raw = flag
        .map(str::to_owned)
        .or_else(|| std::env::var("MOA_API_URL").ok())
        .unwrap_or_else(|| DEFAULT_SERVER.to_owned());
    let trimmed = raw.trim().trim_end_matches('/');
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return Err(Failure::usage(format!("Server must be an http(s) URL: {raw}")));
    }
    Ok(trimmed.to_owned())
}

// ---------------------------------------------------------------------------
// 토큰 보관
// ---------------------------------------------------------------------------

fn config_dir() -> Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(xdg).join("moa-submitter"));
    }
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Failure::service("Cannot find your home directory ($HOME is not set)."))?;
    Ok(PathBuf::from(home).join(".config").join("moa-submitter"))
}

fn token_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("token"))
}

fn store_token(token: &str) -> Result<PathBuf> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| Failure::service(format!("Cannot create {}: {e}", dir.display())))?;
    let path = dir.join("token");
    // 토큰은 30일짜리 제출 권한이다. 먼저 만들고 권한을 좁힌 뒤에 쓴다.
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|e| Failure::service(format!("Cannot write {}: {e}", path.display())))?;
    writeln!(file, "{token}")
        .map_err(|e| Failure::service(format!("Cannot write {}: {e}", path.display())))?;
    Ok(path)
}

/// 환경변수가 먼저다. CI 나 일회성 실행에서 저장된 토큰을 건드리지 않고 덮어쓸
/// 수 있어야 한다.
fn load_token() -> Result<String> {
    if let Ok(token) = std::env::var("MOA_TOKEN") {
        let token = token.trim().to_owned();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    let path = token_path()?;
    let stored = std::fs::read_to_string(&path).map_err(|_| {
        Failure::rejected("You are not logged in. Run `moa-submitter login` first.")
    })?;
    let token = stored.trim().to_owned();
    if token.is_empty() {
        return Err(Failure::rejected(
            "Your stored token is empty. Run `moa-submitter login` again.",
        ));
    }
    Ok(token)
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// 이 도구가 다시 보내도 안전한 요청에만 쓴다. 제출 업로드에는 쓰지 않는다 --
/// 요청이 닿은 뒤 응답만 끊겼을 수 있어서 다시 보내면 두 번 제출된다.
fn retrying<F>(mut call: F) -> std::result::Result<ureq::Response, ureq::Error>
where
    F: FnMut() -> std::result::Result<ureq::Response, ureq::Error>,
{
    for attempt in 1..CONNECT_ATTEMPTS {
        match call() {
            Err(ureq::Error::Transport(_)) => std::thread::sleep(RETRY_DELAY * attempt),
            other => return other,
        }
    }
    call()
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(HTTP_TIMEOUT)
        .user_agent(concat!("moa-submitter/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// ureq 는 4xx/5xx 를 오류로 던진다. 본문에 서버가 적어 보낸 사람이 읽을 문장이
/// 들어 있으므로, 그걸 꺼내서 그대로 보여준다.
fn unwrap_response(error: ureq::Error, what: &str) -> Failure {
    match error {
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            let message = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|json| json.get("message")?.as_str().map(str::to_owned))
                .unwrap_or_else(|| {
                    let text = body.trim();
                    if text.is_empty() { format!("the server answered with HTTP {code}") } else { text.to_owned() }
                });
            match code {
                401 | 403 if message.contains("token") || code == 401 => Failure::rejected(format!(
                    "{message}\nRun `moa-submitter login` to get a new token."
                )),
                500..=599 => Failure::service(format!("The competition server is having trouble: {message}")),
                _ => Failure::rejected(message),
            }
        }
        ureq::Error::Transport(transport) => Failure::service(format!(
            "Cannot reach the competition server while trying to {what}: {transport}"
        )),
    }
}

// ---------------------------------------------------------------------------
// login
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceStart {
    device_code: String,
    user_code: String,
    verification_url: String,
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct DevicePoll {
    status: String,
    token: Option<String>,
    user: Option<GithubUser>,
}

#[derive(Deserialize)]
struct GithubUser {
    login: String,
}

fn login(server: &str) -> Result<()> {
    let http = agent();
    let start: DeviceStart = retrying(|| http.post(&format!("{server}/api/cli-auth/start")).call())
        .map_err(|e| unwrap_response(e, "start a login"))?
        .into_json()
        .map_err(|e| Failure::service(format!("The server sent a login reply we could not read: {e}")))?;

    println!("Your one-time code: {}\n", start.user_code);
    println!("Enter it at:\n\n  {}\n", start.verification_url);
    open_browser(&start.verification_url);
    println!("Waiting for you to finish in the browser... (Ctrl-C to cancel)");

    // GitHub 이 정한 간격을 따른다. 더 자주 물으면 slow_down 을 받는다.
    let interval = Duration::from_secs(start.interval.unwrap_or(5).clamp(1, 60));
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            return Err(Failure::rejected(
                "Login timed out after 10 minutes. Run `moa-submitter login` to try again.",
            ));
        }
        std::thread::sleep(interval);

        let url = format!("{server}/api/cli-auth/poll?device_code={}", urlencode(&start.device_code));
        let response = match http.get(&url).call() {
            Ok(response) => response,
            // 브라우저에서 승인하는 동안 연결이 한 번 튕겼다고 로그인을 버릴 이유는
            // 없다. 마감 시각까지 계속 물어본다.
            Err(ureq::Error::Transport(_)) => continue,
            // 404 는 만료, 403 은 명단에 없는 계정이다. 둘 다 기다려 봐야 소용없다.
            Err(ureq::Error::Status(404, _)) => {
                return Err(Failure::rejected(
                    "This login request expired. Run `moa-submitter login` to try again.",
                ))
            }
            Err(error) => return Err(unwrap_response(error, "finish the login")),
        };

        let poll: DevicePoll = response
            .into_json()
            .map_err(|e| Failure::service(format!("The server sent a login reply we could not read: {e}")))?;
        if poll.status != "authorized" {
            continue;
        }
        let token = poll
            .token
            .ok_or_else(|| Failure::service("The server authorized the login but sent no token."))?;
        let path = store_token(&token)?;
        let who = poll.user.map(|u| u.login).unwrap_or_else(|| "your GitHub account".into());
        println!("\nLogged in as {who}.");
        println!("Token saved to {} (valid for 30 days).", path.display());
        return Ok(());
    }
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 브라우저가 안 열려도 로그인은 계속된다. URL 은 이미 위에 찍어 두었다.
fn open_browser(url: &str) {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    let _ = std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// ---------------------------------------------------------------------------
// submit
// ---------------------------------------------------------------------------

/// 참가자의 작업 위치와 상관없이 크레이트 루트를 찾는다. `--source` 를 주면 그
/// 디렉토리가 곧 루트이고, 안 주면 현재 위치에서 위로 올라가며 찾는다.
fn find_repository(source: Option<&Path>) -> Result<PathBuf> {
    if let Some(given) = source {
        let root = given.canonicalize().map_err(|e| {
            Failure::usage(format!("Cannot open --source {}: {e}", given.display()))
        })?;
        if !root.join(ALLOWED_FILE).is_file() {
            return Err(Failure::usage(format!(
                "{} is not a baseline checkout: it has no {ALLOWED_FILE}.",
                root.display()
            )));
        }
        return Ok(root);
    }

    let start = std::env::current_dir()
        .and_then(|dir| dir.canonicalize())
        .map_err(|e| Failure::usage(format!("Cannot read the current directory: {e}")))?;
    let mut probe = start.as_path();
    loop {
        if probe.join(ALLOWED_FILE).is_file() {
            return Ok(probe.to_path_buf());
        }
        match probe.parent() {
            Some(parent) => probe = parent,
            None => {
                return Err(Failure::usage(format!(
                    "No baseline repository here. {} and none of its parents contain {ALLOWED_FILE}.\n\
                     Run this inside your clone of the baseline, or pass --source <path>.",
                    start.display()
                )))
            }
        }
    }
}

struct Upload {
    path: String,
    bytes: Vec<u8>,
}

fn collect_sources(root: &Path) -> Result<Vec<Upload>> {
    let mut files = vec![read_upload(root, &root.join(ALLOWED_FILE))?];
    let device = root.join(ALLOWED_DIR);
    if !device.is_dir() {
        return Err(Failure::usage(format!(
            "{}/ is missing from {}. Submit from an unmodified baseline layout.",
            ALLOWED_DIR,
            root.display()
        )));
    }
    walk_device(root, &device, &mut files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));

    if files.len() > MAX_FILES {
        return Err(Failure::usage(format!(
            "A submission may contain at most {MAX_FILES} files, but this one has {}.\n\
             Check whether something unexpected ended up under {ALLOWED_DIR}/.",
            files.len()
        )));
    }
    if let Some(empty) = files.iter().find(|f| f.bytes.is_empty()) {
        return Err(Failure::usage(format!(
            "{} is empty. The server does not accept empty files; delete it or write something in it.",
            empty.path
        )));
    }
    let total: u64 = files.iter().map(|f| f.bytes.len() as u64).sum();
    if total > MAX_TOTAL_BYTES {
        return Err(Failure::usage(format!(
            "The sources total {} KiB, over the {} KiB limit.\n\
             Check whether something unexpected ended up under {ALLOWED_DIR}/.",
            total / 1024,
            MAX_TOTAL_BYTES / 1024
        )));
    }
    Ok(files)
}

fn read_upload(root: &Path, file: &Path) -> Result<Upload> {
    let bytes = std::fs::read(file)
        .map_err(|e| Failure::usage(format!("Cannot read {}: {e}", file.display())))?;
    let relative = file
        .strip_prefix(root)
        .map_err(|_| Failure::usage(format!("{} is outside the repository", file.display())))?;
    // 서버는 항상 `/` 로 구분된 경로를 받는다.
    let path = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    Ok(Upload { path, bytes })
}

/// 심볼릭 링크는 따라가지 않는다. 서버가 어차피 거부하고, 링크를 따라가면
/// 레포 밖의 파일을 모르는 새 올려 보낼 수 있다.
fn walk_device(root: &Path, dir: &Path, out: &mut Vec<Upload>) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| Failure::usage(format!("Cannot read {}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry
            .map_err(|e| Failure::usage(format!("Cannot read {}: {e}", dir.display())))?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| Failure::usage(format!("Cannot inspect {}: {e}", path.display())))?;
        if meta.is_symlink() {
            eprintln!("skipping symlink {}", path.display());
        } else if meta.is_dir() {
            walk_device(root, &path, out)?;
        } else if meta.is_file() {
            out.push(read_upload(root, &path)?);
        }
        if out.len() > MAX_FILES {
            return Err(Failure::usage(format!(
                "More than {MAX_FILES} files under {ALLOWED_DIR}/. Check what is in there."
            )));
        }
    }
    Ok(())
}

/// 무엇을 보내는지 보여주되 23줄을 쏟지는 않는다. 최상위 항목만 세어서 적으면
/// 예상 밖의 파일이 섞였을 때 개수나 이름이 눈에 띈다.
fn print_manifest(files: &[Upload]) {
    let total: u64 = files.iter().map(|f| f.bytes.len() as u64).sum();
    println!("Files:        {} ({:.1} KiB)", files.len(), total as f64 / 1024.0);
    let under_device = files.iter().filter(|f| f.path.starts_with("src/device/")).count();
    for file in files.iter().filter(|f| !f.path.starts_with("src/device/")) {
        println!("                {}", file.path);
    }
    if under_device > 0 {
        println!("                src/device/  ({under_device} files)");
    }
}

fn submit(server: &str, source: Option<&Path>) -> Result<()> {
    let token = load_token()?;
    let root = find_repository(source)?;
    let files = collect_sources(&root)?;

    println!("Repository:   {}", root.display());
    print_manifest(&files);

    let body = serde_json::json!({
        "files": files
            .iter()
            .map(|file| serde_json::json!({
                "path": file.path,
                "contentBase64": base64::engine::general_purpose::STANDARD.encode(&file.bytes),
            }))
            .collect::<Vec<_>>(),
    });

    // 업로드는 다시 보낼 수 없으므로, 먼저 값싼 요청으로 길을 뚫어 둔다. 첫 연결을
    // 흘리는 망에서는 이 요청이 대신 맞아 주고, 망이 아예 죽어 있으면 여기서 끝나
    // 업로드가 닿았는지 아닌지 알 수 없는 상태가 생기지 않는다. 같은 agent 를 쓰므로
    // 실제 업로드는 방금 세운 연결을 그대로 재사용한다.
    let http = agent();
    let warmed = retrying(|| http.get(&format!("{server}/api/health")).call())
        .map_err(|e| unwrap_response(e, "reach the competition server"))?;
    // 본문을 끝까지 읽어야 연결이 풀로 돌아간다. 버리면 닫히고, 업로드가 새 연결을
    // 열면서 방금 뚫어 둔 길을 못 쓴다.
    let _ = warmed.into_string();

    let response = http
        .post(&format!("{server}/api/submissions"))
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(body)
        // 업로드는 다시 보내지 않는다. 요청이 닿은 뒤 응답만 끊겼을 수 있어서,
        // 다시 보내면 같은 제출이 두 번 들어간다.
        .map_err(|e| match e {
            ureq::Error::Transport(transport) => Failure::service(format!(
                "The upload did not complete: {transport}\n\
                 Check `moa-submitter status` before submitting again -- it may already have \
                 gone through."
            )),
            other => unwrap_response(other, "send the submission"),
        })?;

    #[derive(Deserialize)]
    struct Created {
        submission: Submission,
    }
    let created: Created = response
        .into_json()
        .map_err(|e| Failure::service(format!("The server accepted the upload but its reply was unreadable: {e}")))?;

    println!();
    println!("Submitted as: {}", created.submission.team_name);
    println!("Submission:   {}", created.submission.submission_id);
    println!();
    println!("Track it with:  moa-submitter status");
    println!("Read the log:   moa-submitter log {}", created.submission.submission_id);
    Ok(())
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Submission {
    submission_id: String,
    team_name: String,
    status: String,
    stage: Option<String>,
    #[serde(default)]
    cycles: BTreeMap<String, serde_json::Value>,
    score: Option<f64>,
    error: Option<String>,
    flagged: Option<String>,
    queue_position: Option<u32>,
    created_at: Option<String>,
    finished_at: Option<String>,
}

impl Submission {
    /// 줄을 서 있는 동안에는 몇 번째인지까지 보여준다.
    fn display_status(&self) -> String {
        match self.queue_position {
            Some(position) => format!("{} (#{position})", self.status),
            None => self.status.clone(),
        }
    }
    fn display_score(&self) -> String {
        self.score.map(|s| format!("{s:.4}")).unwrap_or_else(|| "-".into())
    }
}

fn fetch<T: serde::de::DeserializeOwned>(server: &str, path: &str, what: &str) -> Result<T> {
    let token = load_token()?;
    let http = agent();
    retrying(|| {
        http.get(&format!("{server}{path}"))
            .set("Authorization", &format!("Bearer {token}"))
            .call()
    })
    .map_err(|e| unwrap_response(e, what))?
        .into_json()
        .map_err(|e| Failure::service(format!("The server sent a reply we could not read: {e}")))
}

fn status(server: &str, submission: Option<&str>) -> Result<()> {
    match submission {
        Some(id) => {
            #[derive(Deserialize)]
            struct One {
                data: Submission,
            }
            let one: One = fetch(server, &format!("/api/submissions/{}", urlencode(id)), "read the submission")?;
            print_detail(&one.data);
        }
        None => {
            #[derive(Deserialize)]
            struct Many {
                data: Vec<Submission>,
            }
            let many: Many = fetch(server, "/api/submissions", "list your submissions")?;
            if many.data.is_empty() {
                println!("You have no submissions yet. Run `moa-submitter submit` from your repository.");
                return Ok(());
            }
            print_table(&many.data);
        }
    }
    Ok(())
}

fn print_table(rows: &[Submission]) {
    let headers = ["SUBMISSION", "TEAM", "STATUS", "SCORE", "SUBMITTED"];
    let body: Vec<[String; 5]> = rows
        .iter()
        .map(|row| {
            [
                row.submission_id.clone(),
                row.team_name.clone(),
                row.display_status(),
                row.display_score(),
                short_time(row.created_at.as_deref()),
            ]
        })
        .collect();

    let mut widths = headers.map(str::len);
    for row in &body {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }
    let line = |cells: &[String; 5]| {
        cells
            .iter()
            .enumerate()
            .map(|(index, cell)| format!("{cell:<width$}", width = widths[index]))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_owned()
    };
    println!("{}", line(&headers.map(str::to_owned)));
    println!("{}", widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("  "));
    for row in &body {
        println!("{}", line(row));
    }
}

fn print_detail(one: &Submission) {
    println!("Submission:   {}", one.submission_id);
    println!("Team:         {}", one.team_name);
    println!("Status:       {}", one.display_status());
    if let Some(stage) = &one.stage {
        println!("Stage:        {stage}");
    }
    println!("Submitted:    {}", short_time(one.created_at.as_deref()));
    if let Some(finished) = &one.finished_at {
        println!("Finished:     {}", short_time(Some(finished)));
    }
    if !one.cycles.is_empty() {
        println!("\nCycles:");
        // 사전순이 아니라 테스트가 도는 순서로 보여준다. 로그와 나란히 읽게 된다.
        let mut shown = Vec::new();
        for kernel in KERNEL_ORDER {
            if let Some(value) = one.cycles.get(*kernel) {
                println!("  {kernel:<32} {value}");
                shown.push(*kernel);
            }
        }
        for (kernel, value) in &one.cycles {
            if !shown.contains(&kernel.as_str()) {
                println!("  {kernel:<32} {value}");
            }
        }
    }
    if let Some(score) = one.score {
        println!("\nScore:        {score:.4}");
    }
    if let Some(flagged) = &one.flagged {
        println!("\nNot scored:   {flagged}");
    }
    if let Some(error) = &one.error {
        println!("\n{error}");
    }
    println!("\nFull log:     moa-submitter log {}", one.submission_id);
}

/// `2026-09-09T04:12:31.001Z` 를 `2026-09-09 04:12` 로 줄인다. 표에 넣기 위한
/// 것이므로 초는 버린다.
fn short_time(raw: Option<&str>) -> String {
    let Some(raw) = raw else { return "-".into() };
    if raw.len() >= 16 && raw.as_bytes()[10] == b'T' {
        format!("{} {}", &raw[..10], &raw[11..16])
    } else {
        raw.to_owned()
    }
}

// ---------------------------------------------------------------------------
// log
// ---------------------------------------------------------------------------

fn show_log(server: &str, submission: &str) -> Result<()> {
    let token = load_token()?;
    let encoded = urlencode(submission);
    let http = agent();
    let text = retrying(|| {
        http.get(&format!("{server}/api/submissions/{encoded}/log"))
            .set("Authorization", &format!("Bearer {token}"))
            .call()
    })
    .map_err(|e| unwrap_response(e, "read the log"))?
        .into_string()
        .map_err(|e| Failure::service(format!("The server sent a log we could not read: {e}")))?;

    // 상태를 함께 읽는다. 로그가 비어 있거나 중간에서 끊겨 있을 때, 도구가
    // 고장난 것인지 아직 진행 중인 것인지 참가자가 알 수 있어야 한다.
    #[derive(Deserialize)]
    struct One {
        data: Submission,
    }
    let one: One = fetch(server, &format!("/api/submissions/{encoded}"), "read the submission")?;
    let state = &one.data;

    let body = text.trim_end();
    if body.is_empty() {
        println!("No output yet — status: {}", state.display_status());
        return Ok(());
    }
    println!("{body}");

    match state.status.as_str() {
        "completed" | "failed" => {}
        _ => println!(
            "\n— {} · output appears as each stage finishes. Run this again for more.",
            state.display_status()
        ),
    }
    Ok(())
}
