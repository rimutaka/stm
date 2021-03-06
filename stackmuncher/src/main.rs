use crate::config::AppConfig;
use futures::stream::{FuturesUnordered, StreamExt};
use path_absolutize::{self, Absolutize};
use stackmuncher_lib::{code_rules::CodeRules, config::Config, git, report::Report, utils::hash_str_sha1};
use std::path::Path;
use tracing::{debug, info, warn};

mod app_args;
mod config;
mod help;
mod signing;
mod submission;

#[tokio::main]
async fn main() -> Result<(), ()> {
    // get input params
    let config = AppConfig::new().await;

    tracing_subscriber::fmt()
        .with_max_level(config.lib_config.log_level.clone())
        .with_ansi(false)
        //.without_time()
        .init();

    info!(
        "StackMuncher started in {} from {}",
        config.lib_config.project_dir.to_string_lossy(),
        std::env::current_exe()
            .expect("Cannot get path to stackmuncher executable. It's a bug.")
            .to_string_lossy()
    );

    info!(
        "Report folder: {}",
        config
            .lib_config
            .report_dir
            .as_ref()
            .expect("Cannot unwrap config.report_dir. It's a bug.")
            .absolutize()
            .expect("Cannot convert config.report_dir to absolute path. It's a bug.")
            .to_string_lossy()
    );

    info!(
        "Code rules folder: {}",
        config
            .lib_config
            .code_rules_dir
            .absolutize()
            .expect("Cannot convert config.code_rules_dir to absolute path. It's a bug.")
            .to_string_lossy()
    );

    #[cfg(debug_assertions)]
    info!("Running in debug mode");

    let instant = std::time::Instant::now();

    // load code rules
    let mut code_rules = CodeRules::new(&config.lib_config.code_rules_dir);

    // Reports are grouped per project with a canonical project name as the last subfolder
    let report_dir = Path::new(
        config
            .lib_config
            .report_dir
            .as_ref()
            .expect("Cannot unwrap config.report_dir. It's a bug."),
    );
    warn!("Reports folder: {}", report_dir.to_string_lossy());

    // load a previously generated report if it exists
    let project_report_filename =
        report_dir.join([Config::PROJECT_REPORT_FILE_NAME, Config::REPORT_FILE_EXTENSION].concat());
    let cached_project_report = Report::from_disk(&project_report_filename);

    // get and retain a copy of the full git lot to re-use in multiple places
    let git_log = git::get_log(&config.lib_config.project_dir, None).await?;

    let project_report = match Report::process_project(
        &mut code_rules,
        &config.lib_config.project_dir,
        &cached_project_report,
        &config.lib_config.git_remote_url_regex,
        Some(git_log.clone()),
    )
    .await?
    {
        None => {
            // there were no changes since the previous report - it can be reused as-is
            info!("Done in {}ms", instant.elapsed().as_millis());
            // do not print the end user msg if the logging is enabled
            if config.lib_config.log_level == tracing::Level::ERROR {
                println!("StackMuncher: no new commits since the last report.");
            }
            cached_project_report.expect("Cannot unwrap cached report. It's a bug.")
        }
        Some(v) => {
            let _ = v.save_as_local_file(&project_report_filename);
            info!("Project stack analyzed in {}ms", instant.elapsed().as_millis());
            v
        }
    };

    // check if there are multiple contributors and generate individual reports
    if let Some(contributors) = &project_report.contributors {
        let last_commit_author = project_report.last_commit_author.as_ref().unwrap().clone();

        // prepare a combined list of commit IDs from all known identities
        let list_of_commits = git::get_contributor_commits_from_log(&git_log, &config.lib_config.git_identities);

        // prepare a container for async submission jobs
        let mut submission_jobs = FuturesUnordered::new();

        // a container for the combined contributor report if there are multiple identities
        // we save all identities (for a single contributor) separate and then combine them into a single report
        let mut contributor_reports: Vec<(Report, String)> = Vec::new();

        for contributor in contributors {
            // only process known local identities
            if !config
                .lib_config
                .git_identities
                .contains(&contributor.git_id.trim().to_lowercase())
            {
                debug!("Contributor {} skipped / unknown identity", contributor.git_id);
                continue;
            }

            let contributor_instant = std::time::Instant::now();
            // load the previous contributor report, if any
            let contributor_hash = hash_str_sha1(contributor.git_id.as_str());
            let contributor_report_filename = report_dir.join(
                [
                    Config::CONTRIBUTOR_REPORT_FILE_NAME,
                    contributor_hash.as_str(),
                    Config::REPORT_FILE_EXTENSION,
                ]
                .concat(),
            );

            let cached_contributor_report = Report::from_disk(&contributor_report_filename);

            // only process a single contributor of the latest commit if it's a single commit report update
            if project_report.is_single_commit && contributor.git_id != last_commit_author {
                if let Some(cached_contributor_report) = cached_contributor_report {
                    debug!("Used cached report for contributor {} / single commit", contributor.git_id);

                    // execute multiple submissions concurrently
                    submission_jobs.push(submission::submit_report(
                        &contributor.git_id,
                        cached_contributor_report.clone(),
                        &config.lib_config,
                    ));
                    contributor_reports.push((cached_contributor_report, contributor.git_id.clone()));
                    continue;
                }
                debug!("Missing cached report for contributor {} / single commit", contributor.git_id);
            }

            let contributor_report = project_report
                .process_contributor(
                    &mut code_rules,
                    &config.lib_config.project_dir,
                    &cached_contributor_report,
                    contributor,
                    project_report.tree_files.as_ref(),
                )
                .await?;

            contributor_report.save_as_local_file(&contributor_report_filename);

            info!(
                "Contributor stack for {} analyzed in {}ms",
                contributor.git_id,
                contributor_instant.elapsed().as_millis()
            );

            // push the contributor report into a container to combine later
            contributor_reports.push((contributor_report, contributor.git_id.clone()));
        }

        // combine multiple contributor reports from different identities
        debug!("Combining {} contributor reports", contributor_reports.len());
        if !contributor_reports.is_empty() {
            // seed the combined report from the 1st contributor report in the list of all contributor reports
            let (mut combined_report, contributor_git_id) = contributor_reports.pop().unwrap();
            combined_report.reset_combined_contributor_report(contributor_git_id, &list_of_commits, &project_report);
            for (contributor_report, contributor_git_id) in contributor_reports.into_iter() {
                // this only adds per-file-tech and does not affect any other part of the report
                combined_report.merge_same_project_contributor_reports(contributor_report, contributor_git_id);
            }

            // combine all added per-file-tech into appropriate tech records
            combined_report.recompute_tech_section();

            // let the submission to the directory run concurrently with saving the file
            //
            // TEMPORARY PLUG DO NOT COMMIT !!!
            //
            if let Some(current_identity) = config.lib_config.git_identities.iter().next() {
                submission_jobs.push(submission::submit_report(
                    current_identity,
                    combined_report.clone(),
                    &config.lib_config,
                ));
            }
            // save the combined report
            combined_report.save_as_local_file(
                &report_dir.join(
                    [
                        Config::COMBINED_CONTRIBUTOR_REPORT_FILE_NAME,
                        Config::REPORT_FILE_EXTENSION,
                    ]
                    .concat(),
                ),
            );
        }

        // there should be only a single submission of the combined report
        match submission_jobs.next().await {
            Some(_) => {
                debug!("Combined contributor report submitted");
            }
            None => {
                debug!("No combined contributor report was submitted");
            }
        }
    }
    info!("Repo processed in {}ms", instant.elapsed().as_millis());
    // do not print the end user msg if the logging is enabled
    if config.lib_config.log_level == tracing::Level::ERROR {
        println!("StackMuncher: reports saved in {}", report_dir.to_string_lossy());
    }
    Ok(())
}
