use std::cmp::Ordering;
use std::collections::btree_map::BTreeMap;
use itertools::Itertools;
use tracing::{info};
use serde::{Deserialize, Serialize};
use crate::kernel::{CommandData, CommandPackage};
use crate::kernel::io_handler::IOHandler;
use crate::kernel::lsm::{Manifest, MetaInfo, Position};
use crate::kernel::lsm::lsm_kv::Config;
use crate::kernel::Result;
use crate::KvsError;

/// SSTable
pub(crate) struct SsTable {
    // 表索引信息
    meta_info: MetaInfo,
    // 字段稀疏索引
    sparse_index: BTreeMap<Vec<u8>, Position>,
    // 文件IO操作器
    io_handler: IOHandler,
    // 文件路径
    gen: i64,
    // 数据范围索引
    score: Score
}

/// 数据范围索引
/// 用于缓存SSTable中所有数据的第一个和最后一个数据的Key
/// 标明数据的范围以做到快速区域定位
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct Score {
    start: Vec<u8>,
    end: Vec<u8>
}

impl PartialEq<Self> for SsTable {
    fn eq(&self, other: &Self) -> bool {
        self.meta_info.eq(&other.meta_info)
    }
}

impl PartialOrd<Self> for SsTable {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Option::from(self.get_gen().cmp(&other.get_gen()))
    }
}

impl Score {

    /// 由CommandData组成的Key构成Score
    pub(crate) fn from_cmd_data(first: &CommandData, last: &CommandData) -> Self {
        Score {
            start: first.get_key_clone(),
            end: last.get_key_clone()
        }
    }

    /// 将多个Score重组融合成一个Score
    pub(crate) fn fusion(vec_score :Vec<&Score>) -> Result<Self> {
        if vec_score.len() > 0 {
            let start = vec_score.iter()
                .map(|score| &score.start)
                .sorted()
                .next().unwrap()
                .clone();
            let end = vec_score.iter()
                .map(|score| &score.end)
                .sorted()
                .last().unwrap()
                .clone();

            Ok(Score { start, end })
        } else {
            Err(KvsError::DataEmpty)
        }
    }

    /// 判断Score之间是否相交
    pub(crate) fn meet(&self, target: &Score) -> bool {
        (self.start.le(&target.start) && self.end.gt(&target.start)) ||
            (self.start.lt(&target.end) && self.end.ge(&target.end))
    }

    /// 由一组Command组成一个Score
    pub(crate) fn from_vec_cmd_data(vec_mem_data: &Vec<CommandData>) -> Result<Self> {
        match vec_mem_data.as_slice() {
            [first, .., last] => {
                Ok(Self::from_cmd_data(first, last))
            },
            [one] => {
                Ok(Self::from_cmd_data(one, one))
            },
            _ => {
                Err(KvsError::DataEmpty)
            },
        }
    }

    /// 由一组SSTable组成一组Score
    pub(crate) fn get_vec_score<'a>(vec_ss_table :&'a Vec<&SsTable>) -> Vec<&'a Score> {
        vec_ss_table.iter()
            .map(|ss_table| ss_table.get_score())
            .collect_vec()
    }

    /// 由一组SSTable融合成一个Score
    pub(crate) fn fusion_from_vec_ss_table(vec_ss_table :&Vec<&SsTable>) -> Result<Self> {
        Self::fusion(Self::get_vec_score(vec_ss_table))
    }
}

impl SsTable {

    /// 通过已经存在的文件构建SSTable
    ///
    /// 使用原有的路径与分区大小恢复出一个有内容的SSTable
    pub(crate) async fn restore_from_file(io_handler: IOHandler) -> Result<Self>{
        let gen = io_handler.get_gen();

        let meta_info = MetaInfo::read_to_file(&io_handler).await?;
        info!("[SsTable: {}][restore_from_file][TableMetaInfo]: {:?}", gen, meta_info);

        let index_pos = meta_info.data_len;
        let index_len = meta_info.index_len as usize;

        if let Some(data) = CommandPackage::from_pos_unpack(&io_handler, index_pos, index_len).await? {
            match data {
                CommandData::Set { key, value } => {
                    let sparse_index = rmp_serde::from_slice(&key)?;
                    let score = rmp_serde::from_slice(&value)?;
                    Ok(SsTable {
                        meta_info,
                        sparse_index,
                        gen,
                        io_handler,
                        score
                    })
                }
                _ => Err(KvsError::NotMatchCmd)
            }
        } else {
            Err(KvsError::KeyNotFound)
        }
    }

    /// 写入CommandData数据段
    async fn write_data_part(vec_cmd_data: &mut Vec<&CommandData>, io_handler: &IOHandler, sparse_index: &mut BTreeMap<Vec<u8>, Position>) -> Result<()> {

        let mut start_pos = 0;
        let mut part_len = 0;
        for (index, cmd_data) in vec_cmd_data.iter().enumerate() {
            let (start, len) = CommandPackage::write_back_real_pos(io_handler, cmd_data).await?;
            if index == 0 {
                start_pos = start;
            }
            part_len += len;
        }

        info!("[SSTable][write_data_part][data_zone]: {} to {}", start_pos, part_len);
        // 获取该段首位数据
        if let Some(cmd) = vec_cmd_data.first() {
            info!("[SSTable][write_data_part][sparse_index]: index of the part: {:?}", cmd.get_key());
            sparse_index.insert(cmd.get_key_clone(), Position { start: start_pos, len: part_len });
        }

        vec_cmd_data.clear();
        Ok(())
    }

    pub(crate) fn level(&mut self, level: u64) {
        self.meta_info.level = level;
    }

    pub(crate) fn get_level(&self) -> usize {
        self.meta_info.level as usize
    }

    pub(crate) fn get_version(&self) -> u64 {
        self.meta_info.version
    }

    pub(crate) fn get_gen(&self) -> i64 {
        self.gen
    }

    pub(crate) fn get_score(&self) -> &Score {
        &self.score
    }

    /// 从该sstable中获取指定key对应的CommandData
    pub(crate) async fn query(&self, key: &Vec<u8>) -> Result<Option<CommandData>> {
        if let Some(position) = Position::from_sparse_index_with_key(&self.sparse_index, key) {
            info!("[SsTable: {}][query][data_zone]: {:?}", self.gen, position);
            // 获取该区间段的数据
            let zone = self.io_handler.read_with_pos(position.start, position.len).await?;

            // 返回该区间段对应的数据结果
            Ok(CommandPackage::find_key_with_zone_unpack(zone.as_slice(), &key).await?)
        } else {
            Ok(None)
        }
    }

    /// 获取SsTable内所有的正常数据
    pub(crate) async fn get_all_data(&self) -> Result<Vec<CommandData>> {
        let info = &self.meta_info;
        let data_len = info.data_len;

        let all_data_u8 = self.io_handler.read_with_pos(0, data_len as usize).await?;
        let vec_cmd_data =
                    CommandPackage::from_zone_to_vec(all_data_u8.as_slice()).await?
            .into_iter()
            .map(CommandPackage::unpack)
            .collect_vec();
        Ok(vec_cmd_data)
    }

    /// 通过一组SSTable收集对应的Gen
    pub(crate) fn collect_gen(vec_ss_table: Vec<&SsTable>) -> Result<Vec<i64>> {
        Ok(vec_ss_table.into_iter()
            .map(SsTable::get_gen)
            .collect())
    }

    /// 获取一组SSTable中第一个SSTable的索引位置
    pub(crate) fn first_index_with_level(vec_ss_table: &Vec<&SsTable>, manifest: &Manifest, level: usize) -> usize {
        match vec_ss_table.first() {
            None => 0,
            Some(first_ss_table) => {
                manifest.get_index(level, first_ss_table.get_gen())
                    .unwrap_or(0)
            }
        }
    }

    /// 通过内存表构建持久化并构建SSTable
    ///
    /// 使用目标路径与文件大小，分块大小构建一个有内容的SSTable
    pub(crate) async fn create_for_immutable_table(config: &Config, io_handler: IOHandler, vec_mem_data: &Vec<CommandData>, level: usize) -> Result<Self> {
        // 获取数据的Key涵盖范围
        let score = Score::from_vec_cmd_data(vec_mem_data)?;
        // 获取地址
        let part_size = config.part_size;
        let gen = io_handler.get_gen();
        let mut vec_cmd = Vec::new();
        let mut sparse_index: BTreeMap<Vec<u8>, Position> = BTreeMap::new();

        // 将数据按part_size一组分段存入
        for cmd_data in vec_mem_data {
            vec_cmd.push(cmd_data);
            if vec_cmd.len() >= part_size as usize {
                Self::write_data_part(&mut vec_cmd, &io_handler, &mut sparse_index).await?;
            }
        }
        // 将剩余的指令当作一组持久化
        if !vec_cmd.is_empty() {
            Self::write_data_part(&mut vec_cmd, &io_handler, &mut sparse_index).await?;
        }

        // 开始对稀疏索引进行伪装并断点处理
        // 获取指令数据段的数据长度
        // 不使用真实pos作为开始，而是与稀疏索引的伪装CommandData做区别
        let cmd_sparse_index = CommandData::Set { key: rmp_serde::to_vec(&sparse_index)?, value: rmp_serde::to_vec(&score)?};
        // 将稀疏索引伪装成CommandData，使最后的MetaInfo位置能够被顺利找到
        let (data_part_len, sparse_index_len) = CommandPackage::write(&io_handler, &cmd_sparse_index).await?;


        // 将以上持久化信息封装为MetaInfo
        let meta_info = MetaInfo{
            level: level as u64,
            version: 0,
            data_len: data_part_len as u64,
            index_len: sparse_index_len as u64,
            part_size
        };
        meta_info.write_to_file(&io_handler).await?;

        io_handler.flush().await?;

        info!("[SsTable: {}][create_form_index][TableMetaInfo]: {:?}", gen, meta_info);
        Ok(SsTable {
            meta_info,
            sparse_index,
            io_handler,
            gen,
            score
        })

    }
}