use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, NaiveDateTime, NaiveTime};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::database::Database;
use crate::git;
use crate::models::{Schedule, ScheduleStatus};

pub async fn execute_command_immediate(
    command: &str,
    mode: &str,
    branch: Option<&str>,
    worktree: bool,
    verbose: bool,
    skip_permissions: bool,
    continue_from_last: bool,
) -> Result<()> {
    let execution_path = if worktree {
        if let Some(branch_name) = branch {
            git::get_worktree_path(branch_name)?
        } else {
            std::env::current_dir()?.to_string_lossy().to_string()
        }
    } else {
        std::env::current_dir()?.to_string_lossy().to_string()
    };

    if verbose {
        println!("Executing command: {command}");
        println!("Mode: {mode}");
        println!("Path: {execution_path}");
    }

    let is_shell_mode = mode.to_lowercase() == "shell";
    let (success, output) = execute_command_internal(
        command,
        is_shell_mode,
        &execution_path,
        skip_permissions,
        continue_from_last,
    )
    .await?;

    println!("\n{output}");

    if !success {
        std::process::exit(1);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn schedule_command(
    db: &Database,
    command: &str,
    time: &str,
    date: &str,
    mode: &str,
    branch: Option<&str>,
    worktree: bool,
    memo: Option<&str>,
    skip_permissions: bool,
    continue_from_last: bool,
) -> Result<()> {
    // Parse date
    let target_date = match date.to_lowercase().as_str() {
        "today" => Local::now().date_naive(),
        "tomorrow" => Local::now().date_naive() + chrono::Duration::days(1),
        _ => NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .context("Invalid date format. Use 'today', 'tomorrow', or 'YYYY-MM-DD'")?,
    };

    // Parse time
    let time_parts: Vec<&str> = time.split(':').collect();
    if time_parts.len() != 2 {
        anyhow::bail!("Invalid time format. Use HH:MM");
    }

    let hour: u32 = time_parts[0].parse().context("Invalid hour")?;
    let minute: u32 = time_parts[1].parse().context("Invalid minute")?;

    if hour >= 24 || minute >= 60 {
        anyhow::bail!("Invalid time. Hour must be 0-23, minute must be 0-59");
    }

    let target_time =
        NaiveTime::from_hms_opt(hour, minute, 0).ok_or_else(|| anyhow::anyhow!("Invalid time"))?;

    let scheduled_datetime = NaiveDateTime::new(target_date, target_time);
    let scheduled_time_str = scheduled_datetime.format("%Y-%m-%dT%H:%M").to_string();

    // Get execution branch
    let execution_branch = if worktree {
        if let Some(branch_name) = branch {
            branch_name.to_string()
        } else {
            git::get_current_branch()
        }
    } else {
        git::get_current_branch()
    };

    // Create schedule
    let schedule = Schedule {
        id: format!(
            "schedule_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ),
        command: command.to_string(),
        scheduled_time: Some(scheduled_time_str.clone()),
        _memo: memo.unwrap_or("").to_string(),
        created_at: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        status: ScheduleStatus::Pending,
        is_shell_mode: mode.to_lowercase() == "shell",
        branch: execution_branch.clone(),
        execution_path: std::env::current_dir()?.to_string_lossy().to_string(),
        claude_skip_permissions: skip_permissions,
        claude_continue_from_last: continue_from_last,
    };

    db.create_schedule(&schedule).await?;

    println!("✅ Schedule created successfully!");
    println!("  Command: {command}");
    println!("  Time: {scheduled_time_str}");
    println!("  Mode: {mode}");
    println!("  Branch: {execution_branch}");

    Ok(())
}

pub async fn execute_command_internal(
    command: &str,
    is_shell_mode: bool,
    execution_path: &str,
    skip_permissions: bool,
    continue_from_last: bool,
) -> Result<(bool, String)> {
    let mut cmd = if is_shell_mode {
        if cfg!(target_os = "windows") {
            let mut cmd = Command::new("cmd");
            cmd.arg("/C").arg(command);
            cmd
        } else {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(command);
            cmd
        }
    } else {
        let mut cmd = Command::new("claude");
        if skip_permissions {
            cmd.arg("--dangerously-skip-permissions");
        }
        if continue_from_last {
            cmd.arg("-c");
        }
        cmd.arg("code").arg(command);
        cmd
    };

    cmd.current_dir(execution_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn()?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();

    let mut output = String::new();

    // Read stdout and stderr
    loop {
        tokio::select! {
            result = stdout_reader.next_line() => {
                match result {
                    Ok(Some(line)) => {
                        output.push_str(&line);
                        output.push('\n');
                    }
                    Ok(None) => break,
                    Err(e) => {
                        output.push_str(&format!("Error reading stdout: {e}\n"));
                        break;
                    }
                }
            }
            result = stderr_reader.next_line() => {
                match result {
                    Ok(Some(line)) => {
                        output.push_str(&line);
                        output.push('\n');
                    }
                    Ok(None) => {},
                    Err(e) => {
                        output.push_str(&format!("Error reading stderr: {e}\n"));
                    }
                }
            }
        }
    }

    let status = child.wait().await?;
    Ok((status.success(), output))
}
