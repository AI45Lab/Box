//! `a3s-box-ci` — thin runner bridging any agent (a3s-code / Claude Code / Codex)
//! to the programmable-CI pipeline: a line-based spec in, a JSON `Report` out.
//!
//! ```text
//!   a3s-box-ci run [SPEC|-]   # run a pipeline; prints Report JSON; exit 0 iff passed
//!   a3s-box-ci sweep          # reclaim crashed-pipeline orphans; prints {"removed":[...]}
//! ```
//!
//! Spec format (line-based; `#` comments; reads stdin when SPEC is `-` or omitted):
//! ```text
//!   image <ref>                 # required
//!   setup <shell...>            # optional (default: true)
//!   env <KEY=VALUE>             # repeatable
//!   concurrency <n>             # default 4
//!   retries <n>                 # infra retries (default 2)
//!   max-output <bytes>          # cap per-step stdout/stderr
//!   cache <dir>                 # content-addressed step cache
//!   step <name> :: <cmd>        # repeatable; >= 1 required
//! ```
//! Output JSON is `Report::to_json()`; the process exits non-zero when the report
//! did not pass, so a shell/agent caller can branch on it.

use a3s_box_sdk::pipeline::{sweep_orphans, warm_base, FileCache, Step, WarmBase};
use std::io::Read;

#[derive(Debug)]
struct Spec {
    image: String,
    setup: String,
    env: Vec<(String, String)>,
    steps: Vec<(String, String)>,
    concurrency: usize,
    retries: Option<usize>,
    max_output: Option<usize>,
    cache: Option<String>,
}

fn parse_spec(text: &str) -> Result<Spec, String> {
    let mut s = Spec {
        image: String::new(),
        setup: "true".into(),
        env: Vec::new(),
        steps: Vec::new(),
        concurrency: 4,
        retries: None,
        max_output: None,
        cache: None,
    };
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, rest) = match line.split_once(char::is_whitespace) {
            Some((k, r)) => (k, r.trim()),
            None => (line, ""),
        };
        let bad = |what: &str| format!("line {}: {what}", n + 1);
        match key {
            "image" => s.image = rest.to_string(),
            "setup" => s.setup = rest.to_string(),
            "cache" => s.cache = Some(rest.to_string()),
            "concurrency" => s.concurrency = rest.parse().map_err(|_| bad("bad concurrency"))?,
            "retries" => s.retries = Some(rest.parse().map_err(|_| bad("bad retries"))?),
            "max-output" => s.max_output = Some(rest.parse().map_err(|_| bad("bad max-output"))?),
            "env" => {
                let (k, v) = rest
                    .split_once('=')
                    .ok_or_else(|| bad("env needs KEY=VALUE"))?;
                s.env.push((k.trim().to_string(), v.to_string()));
            }
            "step" => {
                let (name, cmd) = rest
                    .split_once("::")
                    .ok_or_else(|| bad("step needs `name :: cmd`"))?;
                s.steps
                    .push((name.trim().to_string(), cmd.trim().to_string()));
            }
            other => return Err(bad(&format!("unknown key `{other}`"))),
        }
    }
    if s.image.is_empty() {
        return Err("spec missing required `image`".into());
    }
    if s.steps.is_empty() {
        return Err("spec has no `step`".into());
    }
    Ok(s)
}

fn read_input(arg: Option<&str>) -> std::io::Result<String> {
    match arg {
        None | Some("-") => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            Ok(buf)
        }
        Some(path) => std::fs::read_to_string(path),
    }
}

fn run(spec_arg: Option<&str>) -> i32 {
    let text = match read_input(spec_arg) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("a3s-box-ci: read spec: {e}");
            return 2;
        }
    };
    let spec = match parse_spec(&text) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("a3s-box-ci: spec error: {e}");
            return 2;
        }
    };
    let cache = match &spec.cache {
        Some(dir) => match FileCache::new(dir) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("a3s-box-ci: cache dir: {e}");
                return 2;
            }
        },
        None => None,
    };

    let mut wb = WarmBase::new(spec.image.as_str(), spec.setup.as_str());
    for (k, v) in &spec.env {
        wb = wb.env(k.as_str(), v.as_str());
    }
    if let Some(c) = &cache {
        wb = wb.cache(c);
    }
    if let Some(r) = spec.retries {
        wb = wb.infra_retries(r);
    }
    if let Some(m) = spec.max_output {
        wb = wb.max_output(m);
    }

    let base = match warm_base(wb) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("a3s-box-ci: warm_base: {e}");
            return 3;
        }
    };
    let steps: Vec<Step> = spec
        .steps
        .iter()
        .map(|(name, cmd)| Step::new(name.as_str(), cmd.as_str()))
        .collect();
    let report = base.run_parallel(steps, spec.concurrency);
    println!("{}", report.to_json());
    i32::from(!report.passed) // 0 iff passed
}

fn sweep() -> i32 {
    let removed = sweep_orphans();
    let items: Vec<String> = removed
        .iter()
        .map(|n| format!("\"{}\"", n.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    println!("{{\"removed\":[{}]}}", items.join(","));
    0
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some("run") => run(args.get(2).map(String::as_str)),
        Some("sweep") => sweep(),
        _ => {
            eprintln!("usage: a3s-box-ci <run [SPEC|-] | sweep>");
            2
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_spec() {
        let s = parse_spec(
            "# a pipeline\n\
             image  node:20\n\
             setup git clone $REPO /w && cd /w && npm ci\n\
             env REPO=https://x/y\n\
             env TOKEN=a=b\n\
             concurrency 6\n\
             retries 1\n\
             max-output 4096\n\
             cache .ci\n\
             step lint :: cd /w && npm run lint\n\
             step test :: cd /w && npm test\n",
        )
        .unwrap();
        assert_eq!(s.image, "node:20");
        assert_eq!(s.setup, "git clone $REPO /w && cd /w && npm ci");
        assert_eq!(
            s.env,
            vec![
                ("REPO".to_string(), "https://x/y".to_string()),
                ("TOKEN".to_string(), "a=b".to_string()), // only the FIRST '=' splits
            ]
        );
        assert_eq!(s.concurrency, 6);
        assert_eq!(s.retries, Some(1));
        assert_eq!(s.max_output, Some(4096));
        assert_eq!(s.cache.as_deref(), Some(".ci"));
        assert_eq!(
            s.steps,
            vec![
                ("lint".to_string(), "cd /w && npm run lint".to_string()),
                ("test".to_string(), "cd /w && npm test".to_string()),
            ]
        );
    }

    #[test]
    fn defaults_when_omitted() {
        let s = parse_spec("image alpine\nstep go :: true\n").unwrap();
        assert_eq!(s.setup, "true");
        assert_eq!(s.concurrency, 4);
        assert!(s.retries.is_none() && s.max_output.is_none() && s.cache.is_none());
        assert!(s.env.is_empty());
    }

    #[test]
    fn rejects_bad_specs() {
        assert!(parse_spec("step go :: true").unwrap_err().contains("image"));
        assert!(parse_spec("image alpine")
            .unwrap_err()
            .contains("no `step`"));
        assert!(parse_spec("image alpine\nstep oops")
            .unwrap_err()
            .contains("name :: cmd"));
        assert!(parse_spec("image alpine\nenv NOEQ\nstep g :: true")
            .unwrap_err()
            .contains("KEY=VALUE"));
        assert!(parse_spec("image alpine\nbogus x\nstep g :: true")
            .unwrap_err()
            .contains("unknown key"));
        assert!(parse_spec("image alpine\nconcurrency NaN\nstep g :: true")
            .unwrap_err()
            .contains("concurrency"));
    }
}
