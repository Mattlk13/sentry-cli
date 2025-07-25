use std::borrow::Cow;
use std::fs;
use std::io::{BufRead as _, BufReader};

use anyhow::{Context as _, Result};
use chrono::Utc;
use lazy_static::lazy_static;
use regex::Regex;
use sentry::protocol::{Breadcrumb, ClientSdkInfo, Event};

lazy_static! {
    static ref COMPONENT_RE: Regex = Regex::new(r#"^([^:]+): (.*)$"#).expect("this regex is valid");
}

/// Attaches all logs from a logfile as breadcrumbs to the given event.
pub fn attach_logfile(event: &mut Event<'_>, logfile: &str, with_component: bool) -> Result<()> {
    let f = fs::File::open(logfile).context("Could not open logfile")?;

    // sentry currently requires timestamps for breadcrumbs at all times.
    // Because we might not be able to parse a timestamp from the log file
    // we fall back to either the modified time of the file or if that does
    // not work we use the current timestamp.
    let fallback_timestamp = fs::metadata(logfile)
        .context("Could not get metadata for logfile")?
        .modified()
        .map(Into::into)
        .unwrap_or_else(|_| Utc::now());

    let reader = BufReader::new(f);
    for line in reader.lines() {
        let line = line?;
        let rec = anylog::LogEntry::parse(line.as_bytes());

        let (component, message) = if with_component {
            let (component, message) = rec.component_and_message();
            (component.unwrap_or("log"), message)
        } else {
            ("log", rec.message())
        };

        event.breadcrumbs.values.push(Breadcrumb {
            timestamp: rec.utc_timestamp().unwrap_or(fallback_timestamp).into(),
            message: Some(message.to_owned()),
            category: Some(component.to_owned()),
            ..Default::default()
        })
    }

    if event.breadcrumbs.len() > 100 {
        let skip = event.breadcrumbs.len() - 100;
        event.breadcrumbs.values.drain(..skip);
    }

    Ok(())
}

/// Returns SDK information for sentry-cli.
pub fn get_sdk_info() -> Cow<'static, ClientSdkInfo> {
    Cow::Owned(ClientSdkInfo {
        name: env!("CARGO_PKG_NAME").into(),
        version: env!("CARGO_PKG_VERSION").into(),
        integrations: Vec::new(),
        packages: Vec::new(),
    })
}
