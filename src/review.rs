//! Harness 审计契约：固定评分量表、按轮次留痕及显式返工，执行者不能自行判定验收。
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const RUBRIC_VERSION: &str = "ar-quality-v1";

/// 四项离散分值，Rust 计算总分；速度与 Token 不参与质量评分。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Dimensions {
    pub correctness: u8,
    pub completeness: u8,
    pub compliance: u8,
    pub evidence: u8,
}
impl Dimensions {
    /// 拒绝超出量表的值；全零评价按最低 1 分记，不与未评分混淆。
    pub fn score(&self) -> Result<u8> {
        ensure!(
            self.correctness <= 4
                && self.completeness <= 3
                && self.compliance <= 2
                && self.evidence <= 1,
            "invalid_dimensions: correctness 0..4, completeness 0..3, compliance 0..2, evidence 0..1"
        );
        Ok((self.correctness + self.completeness + self.compliance + self.evidence).max(1))
    }
}

/// 分数不能替代业务验收；即使高分，有阻断问题仍可要求返工。
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Accepted,
    Rework,
}

/// Harness 提交一次审计的完整理由；唯一 ID 供断线后幂等重试。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewInput {
    pub review_id: String,
    pub turn: u32,
    pub rubric_version: String,
    pub reviewer: String,
    pub verdict: Verdict,
    pub dimensions: Dimensions,
    pub summary: String,
    pub evidence: Vec<String>,
}
impl ReviewInput {
    /// 验证统一量表和验收门槛，不读取或替 Harness 判断项目代码。
    pub fn validate(&self) -> Result<u8> {
        identifier(&self.review_id)?;
        ensure!(
            self.rubric_version == RUBRIC_VERSION,
            "unsupported_rubric: expected {RUBRIC_VERSION}"
        );
        ensure!(
            !self.reviewer.trim().is_empty() && self.reviewer.chars().count() <= 100,
            "invalid_reviewer"
        );
        ensure!(
            !self.summary.trim().is_empty() && self.summary.chars().count() <= 1500,
            "invalid_review_summary: require 1..1500 characters"
        );
        ensure!(
            self.evidence.len() <= 16
                && self
                    .evidence
                    .iter()
                    .all(|s| !s.trim().is_empty() && s.chars().count() <= 400),
            "invalid_evidence"
        );
        ensure!(
            self.dimensions.evidence == 0 || !self.evidence.is_empty(),
            "evidence_required: evidence point needs verification records"
        );
        let score = self.dimensions.score()?;
        if self.verdict == Verdict::Accepted {
            ensure!(
                score >= 8
                    && self.dimensions.correctness >= 3
                    && self.dimensions.completeness == 3
                    && self.dimensions.compliance >= 1
                    && self.dimensions.evidence == 1,
                "acceptance_gate: acceptance needs score >=8, correctness >=3, completeness 3, compliance >=1 and evidence 1"
            );
        }
        Ok(score)
    }
}

/// 已接受的审计事件保持原样；时间和总分由服务端生成。
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReviewRecord {
    pub input: ReviewInput,
    pub score: u8,
    pub created_at_ms: i64,
}

/// 显式返工绑定未通过的审计；相同 request_id 重发不会再次运行或重复计数。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReworkInput {
    pub request_id: String,
    pub review_id: String,
    pub message: String,
}
impl ReworkInput {
    /// 返工必须有原审计及具体修正指令，不能使用空提示词触发空轮次。
    pub fn validate(&self) -> Result<()> {
        identifier(&self.request_id)?;
        identifier(&self.review_id)?;
        ensure!(!self.message.trim().is_empty(), "rework_message_empty");
        Ok(())
    }
}

/// 只记录成功接收的返工派发；计数来自事件长度，不由调用方填写。
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReworkRecord {
    pub input: ReworkInput,
    pub from_turn: u32,
    pub turn: u32,
    pub created_at_ms: i64,
}

/// 审计和返工标识限制为可稳定传输的短 ASCII 字符串。
fn identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 80
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid_review_identifier"
    );
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    /// 给测试提供明确通过或需要返工的审计，不调用模型进行自评。
    pub fn sample(id: &str, turn: u32, verdict: Verdict) -> ReviewInput {
        ReviewInput {
            review_id: id.into(),
            turn,
            rubric_version: RUBRIC_VERSION.into(),
            reviewer: "Test Harness".into(),
            verdict,
            dimensions: Dimensions {
                correctness: if verdict == Verdict::Accepted { 4 } else { 2 },
                completeness: 3,
                compliance: 2,
                evidence: 1,
            },
            summary: "基于实际差异与测试结果审计".into(),
            evidence: vec!["确定性测试已执行".into()],
        }
    }
    /// 全零为 1、满分为 10，未完成与缺证据的高分结果不能冒充验收通过。
    #[test]
    fn rubric_boundaries_and_acceptance_gate() {
        assert_eq!(
            Dimensions {
                correctness: 0,
                completeness: 0,
                compliance: 0,
                evidence: 0
            }
            .score()
            .unwrap(),
            1
        );
        let mut review = sample("r", 1, Verdict::Accepted);
        assert_eq!(review.validate().unwrap(), 10);
        review.dimensions.correctness = 5;
        assert!(review.validate().is_err());
        review.dimensions.correctness = 4;
        review.dimensions.evidence = 0;
        assert!(review.validate().is_err());
        review.verdict = Verdict::Rework;
        assert_eq!(review.validate().unwrap(), 9);
        review.rubric_version = "other".into();
        assert!(review.validate().is_err());
    }
    /// 非法字段与空理由立即拒绝，不能由客户端绕过公式直接传总分。
    #[test]
    fn review_does_not_accept_arbitrary_total() {
        let mut value = serde_json::to_value(sample("r", 1, Verdict::Accepted)).unwrap();
        value["score"] = serde_json::json!(10);
        assert!(serde_json::from_value::<ReviewInput>(value).is_err());
        let mut review = sample("r", 1, Verdict::Accepted);
        review.summary.clear();
        assert!(review.validate().is_err());
    }
}
