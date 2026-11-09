//! `jail bench`: what the sandbox costs per call.
//! Runs `true` N times plain and N times jailed (with and without the egress proxy).

use std::fmt;
use std::time::Instant;

use anyhow::Result;

use crate::policy::Policy;
use crate::run::{Io, Jail};

#[derive(Debug, Clone)]
pub struct Bench {
    pub n: usize,
    pub plain: Percentiles,
    pub jailed: Percentiles,
    pub jailed_setup: Percentiles,
    pub jailed_proxy: Percentiles,
}

#[derive(Debug, Clone, Copy)]
pub struct Percentiles {
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

fn pct(mut v: Vec<f64>) -> Percentiles {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| v[((v.len() as f64 - 1.0) * q).round() as usize];
    Percentiles { p50: at(0.5), p95: at(0.95), max: *v.last().unwrap() }
}

pub async fn run(jail: &Jail, n: usize) -> Result<Bench> {
    let n = n.max(3);
    let argv = vec!["true".to_string()];
    let mut plain = vec![];
    for _ in 0..n {
        let t = Instant::now();
        tokio::process::Command::new("true").status().await?;
        plain.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let (mut jailed, mut setup) = (vec![], vec![]);
    for _ in 0..n {
        let r = jail.run(&Policy::default(), &argv, Io::Capture).await?;
        jailed.push(r.wall_ms);
        if let Some(s) = r.setup_ms {
            setup.push(s);
        }
    }
    let pol = Policy { domains: vec!["example.com".into()], ..Default::default() };
    let mut proxied = vec![];
    for _ in 0..n {
        proxied.push(jail.run(&pol, &argv, Io::Capture).await?.wall_ms);
    }
    if setup.is_empty() {
        setup.push(0.0);
    }
    Ok(Bench { n, plain: pct(plain), jailed: pct(jailed), jailed_setup: pct(setup), jailed_proxy: pct(proxied) })
}

impl fmt::Display for Bench {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let row = |f: &mut fmt::Formatter<'_>, name: &str, p: &Percentiles| {
            writeln!(f, "  {name:<28} p50 {:>7.2} ms   p95 {:>7.2} ms   max {:>7.2} ms", p.p50, p.p95, p.max)
        };
        writeln!(f, "jail bench — `true`, {} runs each", self.n)?;
        writeln!(f)?;
        row(f, "plain spawn", &self.plain)?;
        row(f, "jailed", &self.jailed)?;
        row(f, "  of which sandbox setup", &self.jailed_setup)?;
        row(f, "jailed + egress proxy", &self.jailed_proxy)?;
        writeln!(f)?;
        writeln!(f, "  overhead per call (p50): {:.2} ms (no network), {:.2} ms (with proxy)",
                 self.jailed.p50 - self.plain.p50, self.jailed_proxy.p50 - self.plain.p50)
    }
}
