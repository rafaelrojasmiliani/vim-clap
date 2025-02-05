use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use subprocess::Exec;

use filter::{FilterContext, ParSource, Source};
use matcher::{Bonus, ClapItem, FuzzyAlgorithm, MatchScope, MatcherBuilder};

use crate::app::Params;
use crate::paths::AbsPathBuf;

fn parse_bonus(s: &str) -> Bonus {
    if s.to_lowercase().as_str() == "filename" {
        Bonus::FileName
    } else {
        Bonus::None
    }
}

/// Execute the shell command
#[derive(Parser, Debug, Clone)]
pub struct Filter {
    /// Initial query string
    #[clap(index = 1, long)]
    query: String,

    /// Fuzzy matching algorithm
    #[clap(long, parse(from_str), default_value = "fzy")]
    algo: FuzzyAlgorithm,

    /// Shell command to produce the whole dataset that query is applied on.
    #[clap(long)]
    cmd: Option<String>,

    /// Working directory of shell command.
    #[clap(long)]
    cmd_dir: Option<String>,

    /// Recently opened file list for adding a bonus to the initial score.
    #[clap(long, parse(from_os_str))]
    recent_files: Option<PathBuf>,

    /// Read input from a file instead of stdin, only absolute file path is supported.
    #[clap(long)]
    input: Option<AbsPathBuf>,

    /// Apply the filter on the full line content or parial of it.
    #[clap(long, parse(from_str), default_value = "full")]
    match_scope: MatchScope,

    /// Add a bonus to the score of base matching algorithm.
    #[clap(long, parse(from_str = parse_bonus), default_value = "none")]
    bonus: Bonus,

    /// Synchronous filtering, returns until the input stream is complete.
    #[clap(long)]
    sync: bool,

    #[clap(long)]
    par_run: bool,
}

impl Filter {
    /// Firstly try building the Source from shell command, then the input file, finally reading the source from stdin.
    fn generate_source<I: Iterator<Item = Arc<dyn ClapItem>>>(&self) -> Source<I> {
        if let Some(ref cmd_str) = self.cmd {
            if let Some(ref dir) = self.cmd_dir {
                Exec::shell(cmd_str).cwd(dir).into()
            } else {
                Exec::shell(cmd_str).into()
            }
        } else {
            self.input
                .as_ref()
                .map(|i| i.deref().clone().into())
                .unwrap_or(Source::<I>::Stdin)
        }
    }

    fn generate_par_source(&self) -> ParSource {
        if let Some(ref cmd_str) = self.cmd {
            let exec = if let Some(ref dir) = self.cmd_dir {
                Exec::shell(cmd_str).cwd(dir)
            } else {
                Exec::shell(cmd_str)
            };
            ParSource::Exec(Box::new(exec))
        } else {
            let file = self
                .input
                .as_ref()
                .map(|i| i.deref().clone())
                .expect("Only File and Exec source can be parallel");
            ParSource::File(file)
        }
    }

    fn get_bonuses(&self) -> Vec<Bonus> {
        use std::io::BufRead;

        let mut bonuses = vec![self.bonus.clone()];
        if let Some(ref recent_files) = self.recent_files {
            // Ignore the error cases.
            if let Ok(file) = std::fs::File::open(recent_files) {
                let lines: Vec<String> = std::io::BufReader::new(file)
                    .lines()
                    .filter_map(|x| x.ok())
                    .collect();
                bonuses.push(Bonus::RecentFiles(lines.into()));
            }
        }

        bonuses
    }

    pub fn run(
        &self,
        Params {
            number,
            winwidth,
            icon,
            case_matching,
            ..
        }: Params,
    ) -> Result<()> {
        let matcher_builder = MatcherBuilder::default()
            .bonuses(self.get_bonuses())
            .match_scope(self.match_scope)
            .fuzzy_algo(self.algo)
            .case_matching(case_matching);

        if self.sync {
            let ranked = self
                .generate_source::<std::iter::Empty<_>>()
                .matched_items(matcher_builder.build(self.query.as_str().into()))?
                .par_sort()
                .inner();

            printer::print_sync_filter_results(ranked, number, winwidth.unwrap_or(100), icon);
        } else if self.par_run {
            filter::par_dyn_run(
                &self.query,
                FilterContext::new(icon, number, winwidth, matcher_builder),
                self.generate_par_source(),
            )?;
        } else {
            filter::dyn_run::<std::iter::Empty<_>>(
                &self.query,
                FilterContext::new(icon, number, winwidth, matcher_builder),
                self.generate_source(),
            )?;
        }
        Ok(())
    }
}
