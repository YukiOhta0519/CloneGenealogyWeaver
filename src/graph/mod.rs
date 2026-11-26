// Git もデータベースも触らないのは，core と同じく値を受け取って値を返す関数にし，
// リポジトリを作らずにテストを書くためである．短いハッシュを直すのは record が git/ を呼んで行う．

pub mod edges;
pub mod input;
pub mod order;

/// u32 をそのまま使わないのは，版の番号を行番号や片の番号と取り違えたときにコンパイルで止めるためである．
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct VersionIdx(pub u32);

impl VersionIdx {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// 行番号を持たないのは，誤りの文にしか使わず，再開で表から作るときには分からないからである．
pub struct Graph {
    pub versions: Vec<Version>,
    pub order: Vec<VersionIdx>,
    pub position: Vec<u32>,
}

pub struct Version {
    pub commit: String,
    pub parents: Vec<VersionIdx>,
    pub children: Vec<VersionIdx>,
}

impl Graph {
    /// 子の一覧を受け取らずに親から作るのは，新しく記録するときと再開で表から作るときを同じ形にするためである
    pub fn new(versions: Vec<(String, Vec<VersionIdx>)>) -> Result<Graph, VersionIdx> {
        let mut children = vec![Vec::new(); versions.len()];
        for (i, (_, parents)) in versions.iter().enumerate() {
            for parent in parents {
                children[parent.index()].push(VersionIdx(i as u32));
            }
        }
        let versions: Vec<Version> = versions
            .into_iter()
            .zip(children)
            .map(|((commit, parents), children)| Version {
                commit,
                parents,
                children,
            })
            .collect();
        let order = order::topological(&versions)?;
        let mut position = vec![0; versions.len()];
        for (k, v) in order.iter().enumerate() {
            position[v.index()] = k as u32;
        }
        Ok(Graph {
            versions,
            order,
            position,
        })
    }
}
