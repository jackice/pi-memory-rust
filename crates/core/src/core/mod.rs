//! 核心业务逻辑：recall 合并排序、结构化提取、会话整合。
//! 对应 TS 版 `src/core/`，行为逐位一致（bit-exact 契约）。

pub mod consolidation;
pub mod extraction;
pub mod recall;
