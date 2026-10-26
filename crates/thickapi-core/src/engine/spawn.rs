use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;

use anyhow::{Context, Result};

use super::job::JobObject;
use crate::adapters::{SpawnPlan, StdinContent};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn spawn_child(plan: &SpawnPlan, job: &JobObject) -> Result<Child> {
    let mut cmd = Command::new(&plan.program);
    cmd.args(&plan.args)
        .envs(&plan.env)
        .current_dir(&plan.cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    match &plan.stdin {
        StdinContent::Prompt(_) => {
            cmd.stdin(Stdio::piped());
        }
        StdinContent::None => {
            cmd.stdin(Stdio::null());
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", plan.program.display()))?;
    job.assign(&child)
        .with_context(|| "assign child to job object")?;

    Ok(child)
}

/// Feed the prompt to the child's stdin on its own thread, then close the pipe.
/// A blocking write on the caller's thread deadlocks once the prompt is larger than the pipe
/// buffer and the child writes output before draining stdin: the child blocks on a full stdout
/// pipe that nobody is reading yet, and the parent blocks on a full stdin pipe (B6). The caller
/// starts the reader threads and the watchdog first, so the deadline covers this write.
pub fn spawn_stdin_writer(child: &mut Child, plan: &SpawnPlan) -> Option<JoinHandle<()>> {
    let StdinContent::Prompt(prompt) = &plan.stdin else {
        return None;
    };
    let mut stdin = child.stdin.take()?;
    let prompt = prompt.clone();
    Some(std::thread::spawn(move || {
        // A killed or early-exiting child breaks the pipe; that is the watchdog's business.
        let _ = stdin.write_all(prompt.as_bytes());
        let _ = stdin.flush();
    }))
}
