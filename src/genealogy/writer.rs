use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::params;

use crate::genealogy::links::{BirthKind, Role, VersionId, Walk};
use crate::genealogy::tips::Summary;
use crate::process::Interrupt;
use crate::store;
use crate::version::{GENEALOGY_SCHEMA_VERSION, TOOL_VERSION};

/// 割り込みを見る間の行の数
const CHECK_EVERY: usize = 10_000;

pub struct Source<'a> {
    pub run_id: &'a str,
    pub path: &'a str,
}

/// 書き終える前に止まったときに，一時ファイルを消す
struct Temporary {
    path: PathBuf,
    done: bool,
}

impl Drop for Temporary {
    fn drop(&mut self) {
        if !self.done {
            let _ = std::fs::remove_file(&self.path);
            let mut journal = OsString::from(self.path.as_os_str());
            journal.push("-journal");
            let _ = std::fs::remove_file(journal);
        }
    }
}

fn kind_text(kind: BirthKind) -> &'static str {
    match kind {
        BirthKind::Appear => "appear",
        BirthKind::LeftCensored => "left_censored",
    }
}

/// 隣の一時ファイルに書いてから名前を替えるのは，系譜のファイルの名前で置かれるのを，いつも書き終えたものにするためである．
pub fn write(
    output: &Path,
    source: &Source,
    times: &[(VersionId, i64)],
    walk: &Walk,
    summary: &Summary,
    interrupt: &Interrupt,
) -> Result<()> {
    let mut name = OsString::from(output.as_os_str());
    name.push(format!(".tmp-{}", store::new_run_id()));
    let mut temporary = Temporary {
        path: PathBuf::from(name),
        done: false,
    };
    let mut conn = store::create_genealogy(&temporary.path)?;
    let tx = conn.transaction()?;
    store::create_genealogy_schema(&tx)?;
    let mut rows = 0usize;
    let mut tick = || -> Result<()> {
        rows += 1;
        if rows.is_multiple_of(CHECK_EVERY) {
            interrupt.check()?;
        }
        Ok(())
    };

    tx.execute(
        "INSERT INTO genealogy_run VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            store::new_run_id(),
            source.run_id,
            source.path,
            GENEALOGY_SCHEMA_VERSION,
            TOOL_VERSION,
            store::now()
        ],
    )?;
    {
        let mut insert = tx.prepare("INSERT INTO version_time VALUES (?1, ?2)")?;
        for &(version, time) in times {
            insert.execute(params![version, store::format_time(time)?])?;
            tick()?;
        }
    }
    {
        let mut insert =
            tx.prepare("INSERT INTO genealogy VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)")?;
        for (i, g) in summary.genealogies.iter().enumerate() {
            insert.execute(params![
                i as i64 + 1,
                g.root_count,
                g.birth.map(|b| b.0),
                g.birth.map(|b| kind_text(b.1)),
                g.censored,
                g.effective_days,
                g.dag_transitions,
                g.dag_transitions_with_skipped,
            ])?;
            tick()?;
        }
    }
    let id = |node: u32| i64::from(summary.of_node[node as usize]) + 1;
    {
        let mut insert = tx.prepare("INSERT INTO genealogy_root VALUES (?1, ?2, ?3, ?4)")?;
        for (node, birth) in walk.births.iter().enumerate() {
            insert.execute(params![
                id(node as u32),
                birth.version,
                birth.pair,
                kind_text(birth.kind)
            ])?;
            tick()?;
        }
    }
    {
        let mut insert = tx.prepare("INSERT INTO genealogy_event VALUES (?1, ?2, ?3)")?;
        for event in &walk.events {
            let role = event.role.map(|role| match role {
                Role::Birth => "birth",
                Role::End => "end",
            });
            insert.execute(params![event.event, id(event.node), role])?;
            tick()?;
        }
    }
    {
        let mut insert = tx.prepare("INSERT INTO genealogy_censored_tip VALUES (?1, ?2, ?3)")?;
        for tip in &walk.censored {
            insert.execute(params![id(tip.node), tip.version, tip.pair])?;
            tick()?;
        }
    }
    interrupt.check()?;
    tx.commit()?;
    conn.close()
        .map_err(|(_, err)| err)
        .with_context(|| format!("cannot close {}", temporary.path.display()))?;

    // 上書きしない付け替えにするのは，作り始めてから終わるまでに同じ名前のファイルができても，消さないためである
    match rustix::fs::renameat_with(
        rustix::fs::CWD,
        &temporary.path,
        rustix::fs::CWD,
        output,
        rustix::fs::RenameFlags::NOREPLACE,
    ) {
        Ok(()) => {
            temporary.done = true;
            Ok(())
        }
        Err(rustix::io::Errno::EXIST) => {
            bail!("genealogy file {} already exists", output.display())
        }
        Err(err) => Err(std::io::Error::from(err)).with_context(|| {
            format!(
                "cannot rename {} to {}",
                temporary.path.display(),
                output.display()
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::genealogy::links::memory::Memory;
    use crate::genealogy::links::{Root, Version, walk};
    use crate::genealogy::tips::summarize;
    use std::collections::HashMap;

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn an_interrupt_leaves_nothing_and_an_existing_file_is_not_replaced() {
        let dir = std::env::temp_dir().join(format!("cgweave-writer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut memory = Memory::default();
        memory.roots.insert(1, vec![10]);
        let versions = [Version {
            id: 1,
            ok: true,
            root: Some(Root::Censored),
            effective: 0,
        }];
        let mut walked = walk(&versions, &[], &mut memory, &Interrupt::default()).unwrap();
        let summary = summarize(
            &mut walked,
            &HashMap::from([(1, 0)]),
            &HashMap::from([(1, 1)]),
            |_, births| Ok(births),
        )
        .unwrap();
        let output = dir.join("g.db");
        let source = Source {
            run_id: "r",
            path: "/e.db",
        };
        let write_with = |interrupt: &Interrupt| {
            write(&output, &source, &[(1, 0)], &walked, &summary, interrupt)
        };

        let interrupt = Interrupt::default();
        interrupt.set();
        let err = write_with(&interrupt).unwrap_err();
        assert!(err.is::<crate::error::Interrupted>(), "{err}");
        assert!(names(&dir).is_empty(), "{:?}", names(&dir));

        write_with(&Interrupt::default()).unwrap();
        assert_eq!(names(&dir), ["g.db"]);
        let before = std::fs::read(&output).unwrap();
        // 名前を替える段で，既にある系譜のファイルを上書きせずに止まる
        let err = write_with(&Interrupt::default()).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        assert_eq!(names(&dir), ["g.db"]);
        assert_eq!(std::fs::read(&output).unwrap(), before);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
