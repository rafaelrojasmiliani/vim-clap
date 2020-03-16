use anyhow::Result;
use fuzzy_filter::Source;
use structopt::StructOpt;

use crate::cmd::{Cmd, Maple};

/// Combine json and println macro.
macro_rules! println_json {
  ( $( $field:expr ),+ ) => {
    {
      println!("{}", serde_json::json!({ $(stringify!($field): $field,)* }))
    }
  }
}

mod cmd;
mod error;
mod icon;
mod light_command;

pub mod built_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

fn version() {
    println!(
        "{}",
        format!(
            "version {}{}, built for {} by {}.",
            built_info::PKG_VERSION,
            built_info::GIT_VERSION.map_or_else(|| "".to_owned(), |v| format!(" (git {})", v)),
            built_info::TARGET,
            built_info::RUSTC_VERSION
        )
    );
}

impl Maple {
    fn run(self) -> Result<()> {
        match self.command {
            Cmd::Version => {
                version();
            }
            Cmd::RPC => {
                crate::cmd::rpc::run_forever(std::io::BufReader::new(std::io::stdin()));
            }
            Cmd::Filter { query, input, algo } => {
                let source = input.map(Into::into).unwrap_or(Source::Stdin);
                crate::cmd::filter::run(
                    query,
                    source,
                    algo,
                    self.number,
                    self.enable_icon,
                    self.winwidth,
                )?;
            }
            Cmd::Blines { query, input } => {
                crate::cmd::filter::blines(query, input, self.number, self.winwidth)?;
            }
            Cmd::Exec {
                cmd,
                output,
                cmd_dir,
                output_threshold,
            } => {
                crate::cmd::exec::run(
                    cmd,
                    output,
                    output_threshold,
                    cmd_dir,
                    self.number,
                    self.enable_icon,
                )?;
            }
            Cmd::Grep {
                grep_cmd,
                grep_query,
                glob,
                cmd_dir,
            } => {
                crate::cmd::grep::run(
                    grep_cmd,
                    grep_query,
                    glob,
                    cmd_dir,
                    self.number,
                    self.enable_icon,
                )?;
            }
            Cmd::Helptags { meta_info } => crate::cmd::helptags::run(meta_info)?,
            Cmd::RipgrepForerunner { cmd_dir } => {
                crate::cmd::grep::run_forerunner(cmd_dir, self.number, self.enable_icon)?
            }
        }
        Ok(())
    }
}

pub fn main() -> Result<()> {
    let maple = Maple::from_args();

    maple.run()?;

    Ok(())
}
