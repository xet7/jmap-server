/*
 * Copyright (c) 2020-2022, Stalwart Labs Ltd.
 *
 * This file is part of the Stalwart JMAP Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

use std::time::{Duration, SystemTime};

use actix_web::web;
use jmap_sharing::principal::set::JMAPSetPrincipal;
use store::{
    chrono::{self, Datelike, TimeZone},
    config::env_settings::EnvSettings,
    tracing::{debug, error, info},
    ColumnFamily, Store,
};
use tokio::sync::mpsc;

use crate::{
    cluster::IPC_CHANNEL_BUFFER,
    server::{failed_to, UnwrapFailure},
    JMAPServer,
};

pub enum Event {
    PurgeAccounts,
    PurgeBlobs,
    SnapshotLog,
    CompactDb,
    Exit,
}

enum SimpleCron {
    EveryDay { hour: u32, minute: u32 },
    EveryWeek { day: u32, hour: u32, minute: u32 },
}

const TASK_PURGE_ACCOUNTS: usize = 0;
const TASK_PURGE_BLOBS: usize = 1;
const TASK_SNAPSHOT_LOG: usize = 2;
const TASK_COMPACT_DB: usize = 3;

pub fn spawn_housekeeper<T>(
    core: web::Data<JMAPServer<T>>,
    settings: &EnvSettings,
    mut rx: mpsc::Receiver<Event>,
) where
    T: for<'x> Store<'x> + 'static,
{
    let purge_accounts_at = SimpleCron::parse(
        &settings
            .get("schedule-purge-accounts")
            .unwrap_or_else(|| "0 3 *".to_string()),
    );
    let purge_blobs_at = SimpleCron::parse(
        &settings
            .get("schedule-purge-blobs")
            .unwrap_or_else(|| "30 3 *".to_string()),
    );
    let snapshot_log_at = SimpleCron::parse(
        &settings
            .get("schedule-snapshot-log")
            .unwrap_or_else(|| "45 3 *".to_string()),
    );
    let compact_db_at = SimpleCron::parse(
        &settings
            .get("schedule-compact-db")
            .unwrap_or_else(|| "0 4 *".to_string()),
    );
    let max_log_entries: u64 = settings.parse("max-changelog-entries").unwrap_or(10000);

    tokio::spawn(async move {
        debug!("Housekeeper task started.");
        loop {
            let time_to_next = [
                purge_accounts_at.time_to_next(),
                purge_blobs_at.time_to_next(),
                snapshot_log_at.time_to_next(),
                compact_db_at.time_to_next(),
            ];
            let mut tasks_to_run = [false, false, false, false];
            let start_time = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);

            match tokio::time::timeout(time_to_next.iter().min().copied().unwrap(), rx.recv()).await
            {
                Ok(Some(event)) => match event {
                    Event::PurgeAccounts => tasks_to_run[TASK_PURGE_ACCOUNTS] = true,
                    Event::PurgeBlobs => tasks_to_run[TASK_PURGE_BLOBS] = true,
                    Event::SnapshotLog => tasks_to_run[TASK_SNAPSHOT_LOG] = true,
                    Event::CompactDb => tasks_to_run[TASK_COMPACT_DB] = true,
                    Event::Exit => {
                        debug!("Housekeeper task exiting.");
                        return;
                    }
                },
                Ok(None) => {
                    debug!("Houskeeper task exiting.");
                    return;
                }
                Err(_) => (),
            }

            // Check which tasks are due for execution
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            for (pos, time_to_next) in time_to_next.into_iter().enumerate() {
                if start_time + time_to_next.as_secs() <= now {
                    tasks_to_run[pos] = true;
                }
            }

            // Spawn tasks
            for (task_id, do_run) in tasks_to_run.into_iter().enumerate() {
                if !do_run {
                    continue;
                }

                let store = core.store.clone();
                let core = core.clone();

                tokio::spawn(async move {
                    let result = match task_id {
                        TASK_PURGE_ACCOUNTS => {
                            info!("Purging deleted accounts.");
                            core.spawn_worker(move || store.principal_purge()).await
                        }
                        TASK_PURGE_BLOBS => {
                            info!("Purging removed and expired blobs.");
                            core.spawn_worker(move || store.purge_blobs()).await
                        }
                        TASK_SNAPSHOT_LOG => {
                            info!("Compacting changes and Raft logs.");
                            core.spawn_worker(move || store.compact_log(max_log_entries))
                                .await
                        }
                        TASK_COMPACT_DB => {
                            info!("Compacting database.");
                            core.spawn_worker(move || store.db.compact(ColumnFamily::Bitmaps))
                                .await
                        }
                        _ => unreachable!(),
                    };

                    if let Err(err) = result {
                        error!("Error while running housekeeper task: {}", err);
                    }
                });
            }
        }
    });
}

pub fn init_housekeeper() -> (mpsc::Sender<Event>, mpsc::Receiver<Event>) {
    mpsc::channel::<Event>(IPC_CHANNEL_BUFFER)
}

impl SimpleCron {
    pub fn parse(value: &str) -> Self {
        let mut hour = 0;
        let mut minute = 0;

        for (pos, value) in value.split(' ').enumerate() {
            if pos == 0 {
                minute = value.parse::<u32>().failed_to("parse minute.");
                if !(0..=59).contains(&minute) {
                    failed_to(&format!("parse minute, invalid value: {}", minute));
                }
            } else if pos == 1 {
                hour = value.parse::<u32>().failed_to("parse hour.");
                if !(0..=23).contains(&hour) {
                    failed_to(&format!("parse hour, invalid value: {}", hour));
                }
            } else if pos == 2 {
                if value.as_bytes().first().failed_to("parse weekday") == &b'*' {
                    return SimpleCron::EveryDay { hour, minute };
                } else {
                    let day = value.parse::<u32>().failed_to("parse weekday.");
                    if !(1..=7).contains(&hour) {
                        failed_to(&format!(
                            "parse weekday, invalid value: {}, range is 1 (Monday) to 7 (Sunday).",
                            hour,
                        ));
                    }

                    return SimpleCron::EveryWeek { day, hour, minute };
                }
            }
        }

        failed_to("parse cron expression.");
    }

    pub fn time_to_next(&self) -> Duration {
        let now = chrono::Local::now();
        let next = match self {
            SimpleCron::EveryDay { hour, minute } => {
                let next = chrono::Local
                    .ymd(now.year(), now.month(), now.day())
                    .and_hms(*hour, *minute, 0);
                if next < now {
                    next + chrono::Duration::days(1)
                } else {
                    next
                }
            }
            SimpleCron::EveryWeek { day, hour, minute } => {
                let next = chrono::Local
                    .ymd(now.year(), now.month(), now.day())
                    .and_hms(*hour, *minute, 0);
                if next < now {
                    next + chrono::Duration::days(
                        (7 - now.weekday().number_from_monday() + *day).into(),
                    )
                } else {
                    next
                }
            }
        };

        (next - now).to_std().unwrap()
    }
}
