//! Signals besides the graph through the `ripple` binary and the production
//! pipeline: coverage reports (one-shot and daemon reloads), per-file
//! metrics in `impact`, `ripple file` and `ripple rank`.

mod common;

use common::*;
use serde_json::{Value, json};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const ORDER: &str = "\
class Order
  def total(items, coupon)
    sum = 0
    items.each do |i|
      if i.taxable?
        sum += i.price * 1.2
      elsif i.free?
        next
      else
        sum += i.price
      end
    end
    sum -= coupon.amount if coupon && coupon.valid?
    sum
  end

  def ship!
    update(shipped: true)
    notify
  end

  def cancel
    update(cancelled: true)
  end
end
";

/// SimpleCov line counts for ORDER: `total` and `cancel` run, `ship!` never
/// (its `def` line runs when the class loads, as in real reports).
const ORDER_COV: &str = "[1,1,5,5,5,3,2,1,null,1,null,null,5,5,null,null,1,0,0,null,null,1,2,null,null]";
const ORDER_COV_ALL: &str = "[1,1,5,5,5,3,2,1,null,1,null,null,5,5,null,null,1,4,4,null,null,1,2,null,null]";

const CONFIG: &str = "\
version: 1
areas:
  orders:
    paths: [app/models/order.rb]
    tier: high
    owners: ['@shop']
rules:
  - id: poorly_covered
    risk: high
    when: {coverage_under: 90}
  - id: churny
    risk: medium
    when: {churn_over: 2}
";

struct Env {
    repo: Repo,
    sock: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let repo = Repo::new(&[
            ("app/models/order.rb", ORDER),
            ("app/models/user.rb", "class User\n  def name\n    'x'\n  end\nend\n"),
            ("app/services/checkout.rb", "class Checkout\n  def run\n    Order.new.total([], nil)\n  end\nend\n"),
            ("spec/models/order_spec.rb", "describe Order do\n  it { Order.new }\nend\n"),
            (".gitignore", "/coverage\n"),
            ("ripple.yml", CONFIG),
        ]);
        for i in 0..3 {
            repo.write("app/models/order.rb", &format!("{ORDER}# {i}\n"));
            repo.write("app/services/checkout.rb", &format!("class Checkout\n  def run\n    Order.new.total([], nil)\n  end\nend\n# {i}\n"));
            repo.commit(&format!("change {i}"));
        }
        let env = Self { repo, sock: socket_dir() };
        env.coverage(ORDER_COV);
        env
    }
    fn coverage(&self, order_lines: &str) {
        let order = self.repo.root.join("app/models/order.rb");
        let user = self.repo.root.join("app/models/user.rb");
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let body = format!(
            r#"{{"RSpec": {{"coverage": {{"{}": {{"lines": {order_lines}}}, "{}": {{"lines": [1,1,0,null,null]}}}}, "timestamp": {now}}}}}"#,
            order.display(),
            user.display()
        );
        self.repo.write("coverage/.resultset.json", &body);
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ripple"))
            .current_dir(&self.repo.root)
            .env("RIPPLE_SOCKET_DIR", self.sock.path())
            .env_remove("RIPPLE_EDGE_FILE")
            .env_remove("CI")
            .env_remove("GITHUB_ACTIONS")
            .env_remove("RIPPLE_NO_DAEMON")
            .env_remove("RIPPLE_ROOT")
            .args(args)
            .output()
            .unwrap()
    }
    fn text(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    }
    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_str(&self.text(args)).unwrap()
    }
    /// Polls `args` (JSON) until `ok` holds.
    fn wait(&self, args: &[&str], what: &str, ok: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let v = self.json(args);
            if ok(&v) {
                return v;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}: {v:#}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.run(&["daemon", "stop"]);
    }
}

#[test]
fn one_shot_file_rank_and_impact_read_coverage_and_history() {
    let env = Env::new();
    env.text(&["history", "build"]);

    let f = env.json(&["--no-daemon", "file", "app/models/order.rb", "--format", "json"]);
    assert_eq!(f["language"], "ruby");
    assert_eq!(f["areas"], json!([{"area": "orders", "tier": "high", "owners": ["@shop"]}]));
    assert_eq!((f["is_test"].as_bool(), f["is_global"].as_bool()), (Some(false), Some(false)));
    assert_eq!(f["methods"], 3);
    assert_eq!(f["top_methods"][0]["name"], "total");
    assert_eq!((f["top_methods"][0]["start"].as_u64(), f["top_methods"][0]["end"].as_u64()), (Some(2), Some(15)));
    assert!(f["metrics"]["complexity"].as_u64().unwrap() >= f["metrics"]["max_method"].as_u64().unwrap());
    assert_eq!(f["uncovered_methods"], json!(["ship!"]));
    let cov = &f["metrics"]["coverage"];
    assert_eq!((cov["covered"].as_u64(), cov["coverable"].as_u64()), (Some(14), Some(16)), "{cov}");
    assert_eq!(f["metrics"]["churn"]["commits"], 4);
    assert_eq!(f["dependents"]["count"], 1);
    assert_eq!(f["dependents"]["top"][0]["file"], "app/services/checkout.rb");
    assert_eq!(f["tests"]["files"], json!(["spec/models/order_spec.rb"]));
    for s in ["complexity", "churn", "coverage", "hotspot"] {
        assert_eq!(f["signals"][s]["state"], "ready", "{s}: {}", f["signals"]);
    }
    let t = env.text(&["--no-daemon", "file", "app/models/order.rb"]);
    assert!(t.lines().count() <= 40, "{t}");
    assert!(t.contains("most complex methods:") && t.contains("total (lines 2-15)") && t.contains("uncovered methods: ship!"), "{t}");

    let r = env.json(&["--no-daemon", "rank", "--format", "json"]);
    assert_eq!(r["files"], 3, "non-test Ruby files with metrics: {r:#}");
    assert!(r["formula"].as_str().unwrap().contains("percentile"));
    let rows = r["rows"].as_array().unwrap();
    assert_eq!(rows[0]["path"], "app/models/order.rb", "{r:#}");
    assert_eq!(rows[0]["rank"], 1);
    assert!(rows[0]["reason"].as_str().unwrap().contains("churn"), "{}", rows[0]);
    let r = env.json(&["--no-daemon", "rank", "--format", "json", "--path", "app/services/**", "--by", "complexity"]);
    assert_eq!(r["rows"].as_array().unwrap().iter().map(|x| x["path"].as_str().unwrap()).collect::<Vec<_>>(), ["app/services/checkout.rb"]);
    let r = env.json(&["--no-daemon", "rank", "--format", "json", "--limit", "1", "--area", "orders"]);
    assert_eq!(r["rows"].as_array().unwrap().len(), 1);
    assert_eq!(env.run(&["--no-daemon", "rank", "--area", "nope"]).status.code(), Some(2));
    let t = env.text(&["--no-daemon", "rank"]);
    assert!(t.contains("app/models/order.rb") && t.lines().count() <= 2 + 2 * 3 && t.contains("not ready: cochange unavailable"), "{t}");

    let v = env.json(&["--no-daemon", "impact", "--files", "app/models/order.rb", "--format", "json"]);
    assert_schema(&v);
    let m = &v["changed"][0]["metrics"];
    assert_eq!(m["coverage"]["covered"], 14);
    assert_eq!(m["churn"]["commits"], 4);
    assert!(m["hotspot"]["rank"].as_u64().is_some(), "{m}");
    let reasons: Vec<&str> = v["policy"]["reasons"].as_array().unwrap().iter().map(|r| r["rule"].as_str().unwrap()).collect();
    assert!(reasons.contains(&"poorly_covered") && reasons.contains(&"churny"), "{reasons:?}");
    let t = env.text(&["--no-daemon", "impact", "--files", "app/models/order.rb"]);
    assert!(t.contains("\n  cx ") && t.contains(" · churn 4/90d · cov 87.5%"), "{t}");
}

#[test]
fn without_reports_or_history_signal_rules_warn() {
    let env = Env::new();
    std::fs::remove_dir_all(env.repo.root.join("coverage")).unwrap();
    let v = env.json(&["--no-daemon", "impact", "--files", "app/models/order.rb", "--format", "json"]);
    assert_schema(&v);
    assert_eq!(v["signals"]["coverage"]["state"], "unavailable");
    assert_eq!(v["signals"]["churn"]["state"], "unavailable");
    let codes: Vec<&str> = v["warnings"].as_array().unwrap().iter().map(|w| w["code"].as_str().unwrap()).collect();
    assert_eq!(codes.iter().filter(|c| **c == "rule_signal_not_ready").count(), 2, "{codes:?}");
    assert!(v["policy"]["reasons"].as_array().into_iter().flatten().all(|r| r["rule"] != "poorly_covered"));
    let t = env.text(&["--no-daemon", "impact", "--files", "app/models/order.rb"]);
    assert!(t.contains("not ready: churn unavailable"), "{t}");
}

#[test]
fn daemon_loads_and_reloads_coverage() {
    let env = Env::new();
    let args = ["file", "app/models/order.rb", "--format", "json"];
    let f = env.wait(&args, "coverage ready", |f| f["signals"]["coverage"]["state"] == "ready");
    assert_eq!(f["uncovered_methods"], json!(["ship!"]));
    // A new report is picked up without a restart.
    env.coverage(ORDER_COV_ALL);
    let f = env.wait(&args, "reloaded coverage", |f| f["uncovered_methods"] == json!([]));
    assert_eq!(f["metrics"]["coverage"]["covered"], 16);
    // Removing it makes coverage unavailable again.
    std::fs::remove_dir_all(env.repo.root.join("coverage")).unwrap();
    env.wait(&args, "coverage gone", |f| f["signals"]["coverage"]["state"] == "unavailable");
    // rank answers from the daemon (history builds in the background).
    let r = env.wait(&["rank", "--format", "json"], "ranking", |r| r["signals"]["hotspot"]["state"] == "ready");
    assert_eq!(r["files"], 3);
    let s = env.json(&["stats", "--format", "json"]);
    assert!(s["coverage"]["loads"].as_u64().unwrap() >= 2, "{}", s["coverage"]);
    assert!(s["rank"]["computes"].as_u64().unwrap() >= 1, "{}", s["rank"]);
}

#[test]
fn rank_weights_come_from_ripple_yml_and_dependents_exclude_tests() {
    let env = Env::new();
    env.repo.write("ripple.yml", &format!("{CONFIG}rank:\n  weights: {{churn: 0, complexity: 0, dependents: 1, uncovered: 0}}\n"));
    let r = env.json(&["--no-daemon", "rank", "--format", "json"]);
    assert!(r["formula"].as_str().unwrap().contains("(0*churn + 0*complexity + 1*dependents + 0*uncovered)"), "{}", r["formula"]);
    let rows = r["rows"].as_array().unwrap();
    // order.rb: checkout.rb and its spec depend on it; only checkout.rb counts.
    assert_eq!(rows[0]["path"], "app/models/order.rb", "{r:#}");
    assert_eq!(rows[0]["dependents"], 1, "{}", rows[0]);
    assert!(rows[0]["reason"].as_str().unwrap().contains("1 non-test dependents"), "{}", rows[0]);
    assert_eq!(r["hotspot_percent"], 1.0);
}

#[test]
fn rank_and_file_on_a_fresh_daemon_wait_for_signals() {
    let env = Env::new();
    // The first command starts the daemon; history builds in the background.
    let o = env.run(&["rank", "--format", "json"]);
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    let r: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!((r["ready"].as_bool(), r["files"].as_u64()), (Some(true), Some(3)), "{r:#}");
    assert!(!r["rows"].as_array().unwrap().is_empty());
    let f = env.json(&["file", "app/models/order.rb", "--format", "json"]);
    assert_eq!(f["signals"]["churn"]["state"], "ready", "{}", f["signals"]);
    assert!(f["metrics"]["hotspot"]["rank"].as_u64().is_some(), "{}", f["metrics"]);
}
