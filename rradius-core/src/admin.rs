//! Commands an operator runs by hand. They read the server's configuration and talk
//! directly to the NAS, so they need no running server and touch no cluster data.

use crate::common::seal::SealKey;
use crate::config;
use crate::dictionary::Dictionary;
use crate::metrics::Metrics;
use std::path::Path;
use std::sync::Arc;

pub const CHECK_USAGE: &str = "usage: rradius-server --check --config <path>";

/// The exit status of a mistake in a command.
pub const STATUS_MISTAKE: i32 = 2;

pub fn read_seal_key(setting: &str, path: &str) -> Result<SealKey, String> {
    crate::common::seal::read_key_file(std::path::Path::new(path)).map_err(|e| format!("{setting} {e}"))
}

/// Runs the checks a start would run on the configuration, without opening a port, creating
/// the data directory or contacting anything. Prints the result and returns the exit status.
pub fn check(config_path: &str) -> i32 {
    crate::logging::init_for_command();
    match check_configuration(config_path) {
        Ok(()) => {
            println!("configuration ok");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            STATUS_MISTAKE
        }
    }
}

fn check_configuration(config_path: &str) -> Result<(), String> {
    let config = config::load(Path::new(config_path)).map_err(|e| e.to_string())?;
    let mut dictionary = Dictionary::new();
    dictionary.load_from_file(&config.dictionary).map_err(|e| e.to_string())?;
    let script = &config.lua.script;
    let source = std::fs::read_to_string(script).map_err(|e| format!("cannot read script '{}': {}", script.display(), e))?;
    crate::lua_engine::compile_script(&source).map_err(|e| format!("cannot compile script '{}': {}", script.display(), e))?;
    if let Some(dir) = &config.cluster.migrations_dir {
        crate::lua_engine::read_migrations(dir).map_err(|e| e.to_string())?;
    }
    if let Some(auth) = &config.auth {
        crate::auth_chain::AuthChains::check_settings(auth, &dictionary)?;
    }
    if let Some(http) = &config.lua.http {
        crate::script_http::ScriptHttp::new(http, Arc::new(Metrics::default()))?;
    }
    if let Some(spool) = config.radius.as_ref().and_then(|radius| radius.accounting_spool.as_ref()) {
        crate::spool::check_dir(&spool.dir).map_err(|e| format!("radius.accounting_spool.dir: cannot use '{}': {e}", spool.dir.display()))?;
    }
    crate::extension::installed().check_files(&config)?;
    crate::reload::warn_about_short_secrets(&config);
    crate::reload::warn_about_radsec_networks(&config);
    Ok(())
}
