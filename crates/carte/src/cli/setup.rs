use std::process::ExitCode;

use anyhow::bail;

use super::SetupArgs;
use super::prompt::{self, tilde};
use crate::paths::Paths;
use crate::setup::{self, Facts, SetupEnv, check};

pub fn run(paths: &Paths, args: SetupArgs) -> anyhow::Result<ExitCode> {
    let env = SetupEnv::from_system(paths)?;
    if let Some(domain) = &args.share {
        return share(&env, domain, &args.tunnel, args.dry_run);
    }
    let mut facts = Facts::gather(&env);
    if args.no_skill {
        facts.skill = None;
    }
    if args.check {
        let report = check::check(&env, &facts);
        print!("{report}");
        return Ok(if report.ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }

    let plan = if args.uninstall {
        setup::plan_uninstall(&env, &facts)
    } else {
        setup::plan_install(&env, &facts)
    };
    let import_first = !args.uninstall && facts.puma_dev.links > 0 && facts.projects == 0;
    if !args.uninstall && facts.puma_dev.installed() {
        println!(
            "puma-dev is installed. Setup stops and disables its LaunchAgent so carte can take \
             over ports {} and {}, and points /etc/resolver/{} at carte. {} and puma-dev's CA stay; \
             `carte setup --uninstall` shows how to bring puma-dev back.",
            env.machine.http_port,
            env.machine.https_port,
            env.machine.tld,
            tilde(&env.puma_dev_dir),
        );
        if import_first {
            println!(
                "You have {} apps in {} and no carte projects yet: run `carte import puma-dev` first.",
                facts.puma_dev.links,
                tilde(&env.puma_dev_dir)
            );
        }
        println!();
    }

    if !plan.changes_anything() && !args.uninstall {
        println!("Everything is set up already.");
    } else if plan.steps.is_empty() {
        println!("Nothing to undo.");
    } else {
        let sudo = plan.steps.iter().any(|s| s.needs_sudo());
        println!(
            "{}{}:\n",
            if args.uninstall {
                "Uninstall plan"
            } else {
                "Setup plan"
            },
            if sudo {
                " (sudo asks for your password)"
            } else {
                ""
            }
        );
    }
    print!("{}", plan.render(&env, args.dry_run));

    if args.dry_run {
        println!("\nDry run: nothing was changed.");
        return Ok(ExitCode::SUCCESS);
    }
    if plan.steps.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    if !prompt::interactive(false) {
        bail!(
            "setup asks for confirmation and sudo, so it needs a terminal; `--dry-run` shows the plan"
        );
    }
    println!();
    if import_first && !prompt::confirm("Continue without importing from puma-dev?", false)? {
        println!("Nothing changed. Run `carte import puma-dev`, then `carte setup` again.");
        return Ok(ExitCode::SUCCESS);
    }
    if plan.changes_anything() && !prompt::confirm("Run these steps?", true)? {
        println!("Nothing changed.");
        return Ok(ExitCode::SUCCESS);
    }
    if let Err(err) = setup::execute(&plan.steps, &env) {
        eprintln!("carte: {err:#}");
        if !args.uninstall {
            eprintln!("\n{}", check::fallback(&env));
        }
        return Ok(ExitCode::FAILURE);
    }
    if args.uninstall {
        println!("\ncarte is uninstalled.");
    } else {
        println!("\ncarte is set up.");
    }
    Ok(ExitCode::SUCCESS)
}

fn share(env: &SetupEnv, domain: &str, tunnel: &str, dry_run: bool) -> anyhow::Result<ExitCode> {
    let provider = setup::share::Provider::for_domain(domain);
    let facts = setup::share::ShareFacts::gather(env, provider, tunnel);
    let plan = setup::share::plan(env, &facts, domain, tunnel)?;
    println!("Sharing plan:\n");
    print!("{}", plan.render(env, true));
    if dry_run {
        println!("\nDry run: nothing was changed.");
        return Ok(ExitCode::SUCCESS);
    }
    if !prompt::interactive(false) {
        bail!("setup asks for confirmation, so it needs a terminal; `--dry-run` shows the plan");
    }
    println!();
    if !prompt::confirm("Run these steps?", true)? {
        println!("Nothing changed.");
        return Ok(ExitCode::SUCCESS);
    }
    setup::execute(&plan.steps, env)?;
    println!("\nSharing is set up. Try `carte share <app>`.");
    Ok(ExitCode::SUCCESS)
}
