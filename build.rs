use std::env;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo::rerun-if-changed=.git/HEAD");
    println!("cargo::rerun-if-changed=.git/index");
    println!("cargo::rerun-if-changed=.git/packed-refs");
    println!("cargo::rerun-if-changed=.git/refs/heads");
    println!("cargo::rerun-if-changed=.git/refs/tags");
    for path in git_lines(&["ls-files"]) {
        println!("cargo::rerun-if-changed={path}");
    }
    println!("cargo::rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo::rerun-if-env-changed=GHIS_BUILD_GIT_COMMIT");
    println!("cargo::rerun-if-env-changed=GHIS_BUILD_GIT_STATE");
    println!("cargo::rerun-if-env-changed=GHIS_BUILD_GIT_TAG");

    let commit = metadata_override("GHIS_BUILD_GIT_COMMIT")
        .or_else(|| git(&["rev-parse", "--short=12", "HEAD"]))
        .unwrap_or_else(|| "unknown".into());
    let state = metadata_override("GHIS_BUILD_GIT_STATE")
        .or_else(git_state)
        .unwrap_or_else(|| "unknown".into());
    let tag = metadata_override("GHIS_BUILD_GIT_TAG")
        .or_else(|| git(&["describe", "--tags", "--exact-match", "HEAD"]))
        .unwrap_or_else(|| "none".into());

    let source_date_epoch = env::var("SOURCE_DATE_EPOCH")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let epoch = source_date_epoch
        .as_deref()
        .map(|value| {
            value
                .parse::<i64>()
                .unwrap_or_else(|_| panic!("SOURCE_DATE_EPOCH must be an integer, got `{value}`"))
        })
        .unwrap_or_else(current_epoch);

    emit("GHIS_BUILD_GIT_COMMIT", &commit);
    emit("GHIS_BUILD_GIT_STATE", &state);
    emit("GHIS_BUILD_GIT_TAG", &tag);
    emit("GHIS_BUILD_TIME", &format_utc(epoch));
    emit(
        "GHIS_BUILD_SOURCE_DATE_EPOCH",
        source_date_epoch.as_deref().unwrap_or("unset"),
    );
    emit(
        "GHIS_BUILD_TARGET",
        &env::var("TARGET").unwrap_or_else(|_| "unknown".into()),
    );
    emit(
        "GHIS_BUILD_PROFILE",
        &env::var("PROFILE").unwrap_or_else(|_| "unknown".into()),
    );
}

fn metadata_override(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| sanitize(&value))
        .filter(|value| !value.is_empty())
}

fn git(arguments: &[&str]) -> Option<String> {
    let output = Command::new("git").args(arguments).output().ok()?;
    output
        .status
        .success()
        .then(|| sanitize(&String::from_utf8_lossy(&output.stdout)))
        .filter(|value| !value.is_empty())
}

fn git_lines(arguments: &[&str]) -> Vec<String> {
    let Some(output) = Command::new("git").args(arguments).output().ok() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(sanitize)
        .filter(|value| !value.is_empty())
        .collect()
}

fn git_state() -> Option<String> {
    let output = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()?;
    output.status.success().then(|| {
        if output.stdout.is_empty() {
            "clean".into()
        } else {
            "dirty".into()
        }
    })
}

fn sanitize(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn current_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after the Unix epoch")
        .as_secs() as i64
}

fn format_utc(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let seconds = epoch.rem_euclid(86_400);
    let (year, month, day) = civil_date(days);
    let hour = seconds / 3_600;
    let minute = seconds % 3_600 / 60;
    let second = seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_date(days_since_epoch: i64) -> (i64, i64, i64) {
    let days = days_since_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn emit(name: &str, value: &str) {
    println!("cargo::rustc-env={name}={value}");
}
