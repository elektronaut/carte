use std::process::ExitCode;

use anyhow::bail;

use super::SetupArgs;
use super::prompt::{self, tilde};
use crate::paths::Paths;
use crate::setup::{self, Facts, SetupEnv, check};

pub fn run(paths: &Paths, args: SetupArgs) -> anyhow::Result<ExitCode> {
    let mut env = SetupEnv::from_system(paths)?;
    if let Some(domain) = &args.share {
        env.check_tld()?;
        return share(&env, domain, &args.tunnel, args.dry_run);
    }
    let mut facts = Facts::gather(&env);
    let tld = if args.check || args.uninstall {
        None
    } else {
        choose_tld(&env, &facts, &args)?
    };
    if let Some(tld) = &tld {
        env.machine.tld = tld.clone();
    }
    env.check_tld()?;
    if tld.is_some() {
        facts = Facts::gather(&env);
    }
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

    let mut plan = if args.uninstall {
        setup::plan_uninstall(&env, &facts)
    } else {
        setup::plan_install(&env, &facts)
    };
    if tld.is_some() {
        setup::set_tld(&mut plan, &env, &facts)?;
    }
    let import_first = !args.uninstall && facts.puma_dev.links > 0 && facts.projects == 0;
    if !args.uninstall && facts.puma_dev.installed() {
        let resolver = if env.needs_resolver() {
            format!(", and points /etc/resolver/{} at carte", env.machine.tld)
        } else {
            String::new()
        };
        println!(
            "puma-dev is installed. Setup stops and disables its LaunchAgent so carte can take \
             over ports {} and {}{resolver}. {} and puma-dev's CA stay; \
             `carte setup --uninstall` shows how to bring puma-dev back.",
            env.machine.http_port,
            env.machine.https_port,
            tilde(&env.puma_dev_dir),
        );
    }
    if import_first {
        println!(
            "You have {} apps in {} and no carte projects yet: run `carte import puma-dev` first.",
            facts.puma_dev.links,
            tilde(&env.puma_dev_dir)
        );
    }
    if !args.uninstall && (facts.puma_dev.installed() || import_first) {
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

/// The TLD to save: the one given with `--tld`, or, when `config.toml` doesn't set one
/// and puma-dev or an earlier setup used another, the user's answer to keeping it.
fn choose_tld(env: &SetupEnv, facts: &Facts, args: &SetupArgs) -> anyhow::Result<Option<String>> {
    if let Some(tld) = &args.tld {
        let tld = tld.trim().trim_matches('.').to_ascii_lowercase();
        if !setup::is_tld(&tld) {
            bail!("{tld:?} isn't a TLD; try `localhost` or `test`");
        }
        return Ok(Some(tld));
    }
    let previous = setup::previous_tlds(env, facts);
    if env.tld_configured || previous.is_empty() {
        return Ok(None);
    }
    let by = if facts.puma_dev.domains.iter().any(|d| previous.contains(d)) {
        "puma-dev"
    } else {
        "an earlier carte setup"
    };
    let old = &previous[0];
    if args.dry_run || !prompt::interactive(false) {
        bail!(
            "{by} served apps under .{old}. Choose with `--tld {old}` to keep those hostnames \
             (carte answers them through /etc/resolver/{old}), or `--tld localhost` to switch to \
             .localhost, which needs no resolver. Apps answer under .localhost either way."
        );
    }
    println!(
        "{by} served apps under .{old}. carte can keep those hostnames, through \
         /etc/resolver/{old}, or switch to .localhost, which needs no resolver. Apps answer under \
         .localhost either way.\n"
    );
    let keep = prompt::confirm(&format!("Keep .{old}?"), true)?;
    println!();
    Ok(Some(if keep {
        old.clone()
    } else {
        "localhost".into()
    }))
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
