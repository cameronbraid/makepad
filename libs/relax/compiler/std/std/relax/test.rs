//! The test harness of `makepad test <package>`: what rustc's `--test` main does with libtest
//! (library/test: lib.rs run_test, test_result.rs calc_result, console.rs, cli.rs), running in
//! the program. The loader collects the crate's `#[test]` fns and hands their table to `main`;
//! the command line after `--` is libtest's (filters, --exact, --skip, --ignored,
//! --include-ignored, --test-threads, --list, --nocapture, -q).
//! Not here: output capture (every test prints as under --nocapture), benches, --format.

use crate::string::String;
use crate::vec::Vec;
use crate::sync::{mpsc, Arc, Mutex};
use crate::io::Write;

/// One `#[test]` fn (the loader's table; repr(C), filled by the loader).
#[repr(C)]
pub struct Test {
    pub name: *const u8,
    pub name_len: usize,
    /// the test fn (`fn()`), or, with `start`, the `fn() -> T` that `start` runs
    pub run: usize,
    /// `__relax_test_start::<T>` for a test returning a value, else 0
    pub start: usize,
    /// `#[ignore]`
    pub ignore: u32,
    /// 0: no `#[should_panic]`, 1: `#[should_panic]`, 2: with `expected`
    pub should_panic: u32,
    pub expected: *const u8,
    pub expected_len: usize,
    pub file: *const u8,
    pub file_len: usize,
    pub line: u32,
    pub col: u32,
}

#[repr(C)]
pub struct Tests {
    pub tests: *const Test,
    pub len: usize,
    /// main's answer: the exit status (0, or 101 when a test failed)
    pub code: i32,
}

struct Desc {
    name: String,
    run: usize,
    start: usize,
    ignore: bool,
    should_panic: u32,
    expected: String,
    file: String,
    line: u32,
    col: u32,
}

unsafe fn text(p: *const u8, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    String::from_utf8_lossy(unsafe { crate::slice::from_raw_parts(p, n) }).into_owned()
}

/// A test fn returning a value: `report()` must be success (libtest assert_test_result).
#[doc(hidden)]
pub fn __relax_test_start<T: crate::process::Termination>(test: fn() -> T) {
    let code = test().report().to_i32();
    if code != 0 {
        panic!("the test returned a termination value with a non-zero status code ({}) which indicates a failure", code);
    }
}

struct Opts {
    filters: Vec<String>,
    skip: Vec<String>,
    exact: bool,
    ignored: bool,
    include_ignored: bool,
    list: bool,
    /// libtest OutputFormat::Terse (`-q` / `--format=terse`): one character per test
    quiet: bool,
    threads: usize,
    /// The daemon's limit for one test without a result (RELAX_TEST_TIMEOUT seconds)
    timeout: Option<crate::time::Duration>,
}

/// libtest cli.rs get_format: `--format` (pretty, terse; json and junit only on nightly with
/// -Z unstable-options), terse also with `--quiet`.
fn format_terse(v: &str) -> Result<bool, String> {
    match v {
        "pretty" => Ok(false),
        "terse" => Ok(true),
        "json" | "junit" => Err(format!("The \"{}\" format is only accepted on the nightly compiler with -Z unstable-options", v)),
        _ => Err(format!("argument for --format must be pretty, terse, json or junit (was {})", v)),
    }
}

fn parse_args(args: Vec<String>) -> Result<Opts, String> {
    let mut o = Opts { filters: Vec::new(), skip: Vec::new(), exact: false, ignored: false, include_ignored: false, list: false, quiet: false, threads: 0, timeout: None };
    let mut format: Option<bool> = None;
    let mut args = args.into_iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--exact" => o.exact = true,
            "--ignored" => o.ignored = true,
            "--include-ignored" => o.include_ignored = true,
            "--list" => o.list = true,
            "-q" | "--quiet" => o.quiet = true,
            // output is never captured here
            "--nocapture" | "--no-capture" | "--show-output" => {}
            "--skip" => o.skip.push(args.next().ok_or("--skip needs a value")?),
            "--test-threads" => o.threads = args.next().and_then(|v| v.parse().ok()).filter(|&n| n > 0).ok_or("--test-threads must be a positive integer")?,
            _ if a.starts_with("--test-threads=") => o.threads = a[15..].parse().ok().filter(|&n| n > 0).ok_or("--test-threads must be a positive integer")?,
            _ if a.starts_with("--skip=") => o.skip.push(a[7..].into()),
            "--format" => format = Some(format_terse(&args.next().ok_or("--format needs a value")?)?),
            _ if a.starts_with("--format=") => format = Some(format_terse(&a[9..])?),
            _ if a.starts_with('-') => return Err(format!("Unrecognized option: '{}'", a.trim_start_matches('-'))),
            _ => o.filters.push(a),
        }
    }
    // an explicit --format wins over --quiet (get_format: `None if quiet => Terse`)
    if let Some(terse) = format {
        o.quiet = terse;
    }
    if o.threads == 0 {
        o.threads = match crate::env::var("RUST_TEST_THREADS") {
            Ok(v) => v.parse().ok().filter(|&n: &usize| n > 0).ok_or("RUST_TEST_THREADS is `{v}`, should be a positive integer.")?,
            Err(_) => crate::thread::available_parallelism().map_or(1, |n| n.get()),
        };
    }
    Ok(o)
}

enum Outcome {
    Ok,
    Failed(Option<String>),
    Ignored,
}

/// libtest calc_result: the test's panic (payload) against its `#[should_panic]`.
fn calc_result(d: &Desc, panic: Option<&(dyn crate::any::Any + Send)>) -> Outcome {
    match (d.should_panic, panic) {
        (0, None) | (1, Some(_)) => Outcome::Ok,
        (2, Some(err)) => {
            let s = err.downcast_ref::<String>().map(|e| e.as_str()).or_else(|| err.downcast_ref::<&'static str>().copied());
            match s {
                Some(e) if e.contains(d.expected.as_str()) => Outcome::Ok,
                Some(e) => Outcome::Failed(Some(format!(
                    "panic did not contain expected string\n      panic message: {:?}\n expected substring: {:?}",
                    e, d.expected
                ))),
                None => Outcome::Failed(Some(format!(
                    "expected panic with string value,\n found non-string value: `{:?}`\n     expected substring: {:?}",
                    (*err).type_id(), d.expected
                ))),
            }
        }
        (_, None) => {
            let at = if d.file.is_empty() { String::new() } else { format!(" at {}:{}:{}", d.file, d.line, d.col) };
            Outcome::Failed(Some(format!("test did not panic as expected{}", at)))
        }
        (_, Some(_)) => Outcome::Failed(None),
    }
}

fn run_one(d: &Desc) -> Outcome {
    let (run, start) = (d.run, d.start);
    let r = crate::panic::catch_unwind(move || unsafe {
        if start == 0 {
            let f: fn() = crate::mem::transmute(run);
            f()
        } else {
            let f: fn(usize) = crate::mem::transmute(start);
            f(run)
        }
    });
    match r {
        Ok(()) => calc_result(d, None),
        Err(p) => calc_result(d, Some(&*p)),
    }
}

/// Runs the table's tests; the exit status goes to `t.code`. With `RELAX_TEST_SERVE=<socket>`
/// the harness is a test daemon instead (`serve`).
pub fn main(t: &mut Tests) {
    if let Ok(path) = crate::env::var("RELAX_TEST_SERVE") {
        t.code = serve(t, &path);
        return;
    }
    t.code = match parse_args(crate::env::args().skip(1).collect()) {
        Ok(opts) => run(t, &opts, &mut Out { conn: None }),
        Err(e) => {
            eprintln!("error: {}", e);
            101
        }
    };
}

/// Where a run's report goes: stdout, and a daemon request's connection.
struct Out {
    conn: Option<crate::os::unix::net::UnixStream>,
}

impl Out {
    fn put(&mut self, s: &str) {
        print!("{}", s);
        let _ = crate::io::stdout().flush();
        if let Some(c) = &mut self.conn {
            // a client that went away does not stop the run
            if c.write_all(s.as_bytes()).is_err() {
                self.conn = None;
            }
        }
    }
    fn line(&mut self, s: &str) {
        self.put(s);
        self.put("\n");
    }
}

extern "C" {
    fn __relax_live_sync(timeout_ms: u64, buf: *mut u8, cap: usize, len: *mut usize) -> u32;
    fn __relax_test_table() -> *const Tests;
}

/// The test daemon: the program stays loaded and answers requests on a Unix socket, so a
/// test after an edit costs the edit's patch and the test, not a fresh start. A request is
/// one line of libtest arguments separated by tabs. Before running it the daemon applies the
/// saved sources (the live watcher's patches); an edit that was refused fails the request
/// instead of testing the previous code. The report streams back and ends with
/// `relax-test-serve: exit <code>`. An edit that needs a restart restarts the process (same
/// command line), which closes the connection; the client retries.
fn serve(t: &Tests, path: &str) -> i32 {
    use crate::io::BufRead;
    let _ = crate::fs::remove_file(path);
    let listener = match crate::os::unix::net::UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("relax-test-serve: cannot listen on {}: {}", path, e);
            return 101;
        }
    };
    eprintln!("relax-test-serve: listening on {}", path);
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let mut request = String::new();
        let read = match conn.try_clone() {
            Ok(c) => crate::io::BufReader::new(c).read_line(&mut request),
            Err(e) => Err(e),
        };
        if read.is_err() {
            continue;
        }
        let args: Vec<String> = request.trim_end_matches(['\r', '\n']).split('\t').filter(|a| !a.is_empty()).map(String::from).collect();
        let mut out = Out { conn: Some(conn) };
        let mut refused = Vec::with_capacity(1 << 16);
        let mut n = 0usize;
        let synced = unsafe { __relax_live_sync(60_000, refused.as_mut_ptr(), refused.capacity(), &mut n) };
        unsafe { refused.set_len(n) };
        // an edit the running program cannot take: start again with it (same command line);
        // the connection closes and the client asks the restarted daemon
        if synced == 3 {
            out.put(&format!("relax-test-serve: restarting for:\n{}", String::from_utf8_lossy(&refused)));
            drop(out);
            crate::relax::restart_now("");
            eprintln!("relax-test-serve: the restart failed");
            return 101;
        }
        let code = if !refused.is_empty() {
            out.line("relax-test-serve: edits not applied; fix them before testing:");
            out.put(&String::from_utf8_lossy(&refused));
            101
        } else if synced == 1 {
            out.line("relax-test-serve: the saved sources could not be applied in time (a restart may be due)");
            101
        } else {
            match parse_args(args) {
                Ok(mut opts) => {
                    let secs = crate::env::var("RELAX_TEST_TIMEOUT").ok().and_then(|v| v.parse().ok()).unwrap_or(120u64);
                    opts.timeout = Some(crate::time::Duration::from_secs(secs));
                    // the tests as the program is now: ones a live edit added run too
                    let current = unsafe { __relax_test_table() };
                    run(if current.is_null() { t } else { unsafe { &*current } }, &opts, &mut out)
                }
                Err(e) => {
                    out.line(&format!("error: {}", e));
                    101
                }
            }
        };
        if TIMED_OUT.load(crate::sync::atomic::Ordering::Relaxed) {
            // an abandoned test still runs and may hold what the next one needs: start again
            out.line("relax-test-serve: restarting: a test timed out and was abandoned");
            out.line(&format!("relax-test-serve: exit {}", code));
            drop(out);
            crate::relax::restart_now("");
            eprintln!("relax-test-serve: the restart failed");
            return 101;
        }
        out.line(&format!("relax-test-serve: exit {}", code));
    }
    0
}

/// A daemon request left a test running past its limit (`Opts::timeout`).
static TIMED_OUT: crate::sync::atomic::AtomicBool = crate::sync::atomic::AtomicBool::new(false);

fn run(t: &Tests, opts: &Opts, out: &mut Out) -> i32 {
    let mut all: Vec<Desc> = Vec::with_capacity(t.len);
    for i in 0..t.len {
        let x = unsafe { &*t.tests.add(i) };
        all.push(Desc {
            name: unsafe { text(x.name, x.name_len) },
            run: x.run,
            start: x.start,
            ignore: x.ignore != 0,
            should_panic: x.should_panic,
            expected: unsafe { text(x.expected, x.expected_len) },
            file: unsafe { text(x.file, x.file_len) },
            line: x.line,
            col: x.col,
        });
    }
    // libtest filter_tests: name filters (substring, or --exact), --skip, then --ignored
    let total = all.len();
    let matches = |name: &str, f: &str| if opts.exact { name == f } else { name.contains(f) };
    all.retain(|d| (opts.filters.is_empty() || opts.filters.iter().any(|f| matches(&d.name, f))) && !opts.skip.iter().any(|f| matches(&d.name, f)));
    if opts.ignored {
        all.retain(|d| d.ignore);
        for d in &mut all {
            d.ignore = false;
        }
    } else if opts.include_ignored {
        for d in &mut all {
            d.ignore = false;
        }
    }
    let filtered_out = total - all.len();
    all.sort_by(|a, b| a.name.cmp(&b.name));
    if opts.list {
        for d in &all {
            out.line(&format!("{}: test", d.name));
        }
        if !opts.quiet {
            out.line("");
            out.line(&format!("{} tests, 0 benchmarks", all.len()));
        }
        return 0;
    }
    let started = crate::time::Instant::now();
    out.line("");
    out.line(&format!("running {} test{}", all.len(), if all.len() == 1 { "" } else { "s" }));
    let mut passed = 0usize;
    let mut ignored = 0usize;
    let mut failures: Vec<(String, Option<String>)> = Vec::new();
    // libtest formatters/terse.rs: the tests reported so far and the characters on this line
    let (mut test_count, mut test_column) = (0usize, 0usize);
    let total_count = all.len();
    // libtest's pretty formatter names the mode of a `#[should_panic]` test
    let mut report = |name: &str, should_panic: bool, o: Outcome, failures: &mut Vec<(String, Option<String>)>| {
        let mode = if should_panic { " - should panic" } else { "" };
        let word = match &o {
            Outcome::Ok => "ok",
            Outcome::Failed(_) => "FAILED",
            Outcome::Ignored => "ignored",
        };
        if opts.quiet {
            // TerseFormatter: a character per passed / ignored test, the progress every 88
            // columns, a line per failed one
            const QUIET_MODE_MAX_COLUMN: usize = 88;
            match &o {
                Outcome::Failed(_) => {
                    if test_column != 0 {
                        out.put(&format!(" {}/{}\n", test_count, total_count));
                        test_column = 0;
                    }
                    test_count += 1;
                    out.line(&format!("{} --- FAILED", name));
                }
                _ => {
                    out.put(&format!("{}", if matches!(o, Outcome::Ok) { "." } else { "i" }));
                    test_count += 1;
                    test_column += 1;
                    if test_column % QUIET_MODE_MAX_COLUMN == QUIET_MODE_MAX_COLUMN - 1 {
                        out.put(&format!(" {}/{}\n", test_count, total_count));
                        test_column = 0;
                    }
                }
            }
        } else {
            out.line(&format!("test {}{} ... {}", name, mode, word));
        }
        match o {
            Outcome::Ok => passed += 1,
            Outcome::Ignored => ignored += 1,
            Outcome::Failed(m) => failures.push((String::from(name), m)),
        }
    };
    // libtest run_tests: up to `threads` tests at once, each on a thread named after it
    let descs: Arc<Vec<Desc>> = Arc::new(all);
    let (tx, rx) = mpsc::channel::<(usize, Outcome)>();
    let next = Arc::new(Mutex::new(0usize));
    let mut running = 0usize;
    let mut done = 0usize;
    let n = descs.len();
    let live: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    let spawn = |i: usize, running: &mut usize| {
        live.lock().unwrap().push(i);
        let d = descs.clone();
        let tx = tx.clone();
        let name = d[i].name.clone();
        let h = crate::thread::Builder::new().name(name).spawn(move || {
            let o = run_one(&d[i]);
            let _ = tx.send((i, o));
        });
        match h {
            Ok(_) => *running += 1,
            Err(_) => {
                let o = run_one(&descs[i]);
                let _ = tx.send((i, o));
                *running += 1;
            }
        }
    };
    while done < n {
        while running < opts.threads {
            let i = {
                let mut g = next.lock().unwrap();
                if *g >= n {
                    break;
                }
                // ignored tests are reported without running
                while *g < n && descs[*g].ignore {
                    let k = *g;
                    *g += 1;
                    let _ = tx.send((k, Outcome::Ignored));
                    running += 1;
                }
                if *g >= n {
                    break;
                }
                *g += 1;
                *g - 1
            };
            spawn(i, &mut running);
        }
        let got = match opts.timeout {
            None => rx.recv().ok(),
            Some(limit) => match rx.recv_timeout(limit) {
                Ok(x) => Some(x),
                Err(_) => {
                    // the daemon's limit: the tests still running are reported as failed and left
                    for (k, d) in descs.iter().enumerate() {
                        if live.lock().unwrap().contains(&k) {
                            report(&d.name, d.should_panic != 0, Outcome::Failed(Some(format!("timed out after {} s without a result", limit.as_secs()))), &mut failures);
                        }
                    }
                    TIMED_OUT.store(true, crate::sync::atomic::Ordering::Relaxed);
                    break;
                }
            },
        };
        let Some((i, o)) = got else { break };
        live.lock().unwrap().retain(|&k| k != i);
        running -= 1;
        done += 1;
        report(&descs[i].name, descs[i].should_panic != 0, o, &mut failures);
    }
    // (terse: the line of characters ends with the next section's leading newline)
    let ok = failures.is_empty();
    if !ok {
        out.line("");
        out.line(&format!("failures:"));
        out.line("");
        for (name, m) in &failures {
            if let Some(m) = m {
                out.line(&format!("---- {} stdout ----", name));
                out.line(&format!("{}", m));
                out.line("");
            }
        }
        out.line("");
        out.line(&format!("failures:"));
        for (name, _) in &failures {
            out.line(&format!("    {}", name));
        }
    }
    let secs = started.elapsed().as_secs_f64();
    out.line("");
    out.line(&format!(
        "test result: {}. {} passed; {} failed; {} ignored; 0 measured; {} filtered out; finished in {:.2}s",
        if ok { "ok" } else { "FAILED" },
        passed,
        failures.len(),
        ignored,
        filtered_out,
        secs
    ));
    out.line("");
    if ok { 0 } else { 101 }
}
