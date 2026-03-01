use crate::clover::CloverParser;
use crate::cobertura::CoberturaParser;
use crate::ctrf::CtrfParser;
use crate::jacoco::JacocoParser;
use crate::jest_vitest_json::JestVitestParser;
use crate::junit::JUnitParser;
use crate::lcov::LcovParser;
use crate::playwright_json::PlaywrightJsonParser;
use crate::trx::TrxParser;
use crate::types::{DetectionMatch, ParseError, ParseResult, ResultType};

const DETECTION_THRESHOLD: u8 = 50;

/// A parser that can detect and parse a specific test report format.
///
/// # Detection scores
/// `detect()` returns a confidence score from 0 to 100:
/// - 0: not this format
/// - 50+: confident enough to attempt parsing
/// - 100: certain match
///
/// Scores >= `DETECTION_THRESHOLD` (50) are eligible. On ties, `priority()` breaks it.
pub trait Parser: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn priority(&self) -> u8;
    fn result_type(&self) -> ResultType;
    fn detect(&self, sample: &str, filename: &str) -> u8;
    fn parse(&self, content: &str, filename: &str) -> Result<ParseResult, ParseError>;
}

pub struct ParserRegistry {
    parsers: Vec<Box<dyn Parser>>,
}

impl Default for ParserRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ParserRegistry {
    pub fn new() -> Self {
        ParserRegistry {
            parsers: Vec::new(),
        }
    }

    pub fn register(&mut self, parser: Box<dyn Parser>) -> Result<(), ParseError> {
        let id = parser.id().to_string();
        if self.parsers.iter().any(|p| p.id() == id) {
            return Err(ParseError::from(format!("Duplicate parser id: {}", id)));
        }
        self.parsers.push(parser);
        Ok(())
    }

    pub fn with_defaults() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(JUnitParser))
            .expect("Built-in JUnit parser registration must succeed");
        registry.register(Box::new(JestVitestParser))
            .expect("Built-in Jest/Vitest parser registration must succeed");
        registry.register(Box::new(PlaywrightJsonParser))
            .expect("Built-in Playwright JSON parser registration must succeed");
        registry.register(Box::new(CtrfParser))
            .expect("Built-in CTRF parser registration must succeed");
        registry.register(Box::new(LcovParser))
            .expect("Built-in LCOV parser registration must succeed");
        registry.register(Box::new(TrxParser))
            .expect("Built-in TRX parser registration must succeed");
        registry.register(Box::new(CoberturaParser))
            .expect("Built-in Cobertura parser registration must succeed");
        registry.register(Box::new(JacocoParser))
            .expect("Built-in JaCoCo parser registration must succeed");
        registry.register(Box::new(CloverParser))
            .expect("Built-in Clover parser registration must succeed");
        registry
    }

    /// Detect the best matching parser without parsing.
    /// Returns a `DetectionMatch` describing the winning parser, or `None`.
    pub fn detect(&self, content: &str, filename: &str) -> Option<DetectionMatch> {
        let (best, score) = self.find_best(content, filename)?;
        Some(DetectionMatch {
            parser_id: best.id().to_string(),
            parser_name: best.name().to_string(),
            score,
            result_type: best.result_type(),
        })
    }

    pub fn parse(&self, content: &str, filename: &str) -> Option<Result<ParseResult, ParseError>> {
        let (best, _) = self.find_best(content, filename)?;
        Some(best.parse(content, filename))
    }

    fn find_best(&self, content: &str, filename: &str) -> Option<(&dyn Parser, u8)> {
        self.parsers
            .iter()
            .map(|p| {
                let score = p.detect(content, filename);
                (p.as_ref(), score)
            })
            .filter(|(_, score)| *score >= DETECTION_THRESHOLD)
            .max_by(|(a, a_score), (b, b_score)| {
                a_score.cmp(b_score).then_with(|| a.priority().cmp(&b.priority()))
            })
    }

    pub fn parser_ids(&self) -> Vec<&str> {
        self.parsers.iter().map(|p| p.id()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> ParserRegistry {
        ParserRegistry::with_defaults()
    }

    #[test]
    fn detect_junit_xml() {
        let content = r#"<?xml version="1.0"?><testsuites><testsuite name="t"><testcase name="a"/></testsuite></testsuites>"#;
        let m = registry().detect(content, "report.xml").unwrap();
        assert_eq!(m.parser_id, "junit");
        assert!(m.score >= 50);
        assert_eq!(m.result_type, ResultType::TestReport);
    }

    #[test]
    fn detect_trx() {
        let content = r#"<?xml version="1.0"?><TestRun xmlns="http://microsoft.com/schemas/VisualStudio/TeamTest/2010"><Results></Results></TestRun>"#;
        let m = registry().detect(content, "results.trx").unwrap();
        assert_eq!(m.parser_id, "trx");
        assert!(m.score >= 50);
        assert_eq!(m.result_type, ResultType::TestReport);
    }

    #[test]
    fn detect_ctrf_json() {
        let content = r#"{"results":{"tool":{"name":"vitest"},"summary":{"tests":1},"tests":[{"name":"t","status":"passed","duration":10}]}}"#;
        let m = registry().detect(content, "ctrf-report.json").unwrap();
        assert_eq!(m.parser_id, "ctrf");
        assert!(m.score >= 50);
        assert_eq!(m.result_type, ResultType::TestReport);
    }

    #[test]
    fn detect_jest_vitest_json() {
        let content = r#"{"numTotalTests":1,"numPassedTests":1,"numFailedTests":0,"numPendingTests":0,"testResults":[],"success":true,"startTime":1700000000000}"#;
        let m = registry().detect(content, "results.json").unwrap();
        assert_eq!(m.parser_id, "jest-vitest-json");
        assert!(m.score >= 50);
        assert_eq!(m.result_type, ResultType::TestReport);
    }

    #[test]
    fn detect_playwright_json() {
        let content = r#"{"config":{},"suites":[],"errors":[],"stats":{"startTime":"2024-01-01","duration":100,"expected":0,"unexpected":0,"flaky":0,"skipped":0}}"#;
        let m = registry().detect(content, "results.json").unwrap();
        assert_eq!(m.parser_id, "playwright-json");
        assert!(m.score >= 50);
        assert_eq!(m.result_type, ResultType::TestReport);
    }

    #[test]
    fn detect_lcov() {
        let content = "TN:\nSF:src/file.ts\nDA:1,1\nDA:2,0\nLF:2\nLH:1\nend_of_record\n";
        let m = registry().detect(content, "lcov.info").unwrap();
        assert_eq!(m.parser_id, "lcov");
        assert!(m.score >= 50);
        assert_eq!(m.result_type, ResultType::Coverage);
    }

    #[test]
    fn detect_cobertura_xml() {
        let content = r#"<?xml version="1.0"?><coverage line-rate="0.5" branch-rate="0.3" version="1.0"><packages><package name="p"><classes></classes></package></packages></coverage>"#;
        let m = registry().detect(content, "cobertura.xml").unwrap();
        assert_eq!(m.parser_id, "cobertura");
        assert!(m.score >= 50);
        assert_eq!(m.result_type, ResultType::Coverage);
    }

    #[test]
    fn detect_jacoco_xml() {
        let content = r#"<?xml version="1.0"?><report name="test"><sessioninfo id="s" start="0" dump="0"/><package name="p"><counter type="LINE" missed="1" covered="2"/></package></report>"#;
        let m = registry().detect(content, "jacoco.xml").unwrap();
        assert_eq!(m.parser_id, "jacoco");
        assert!(m.score >= 50);
        assert_eq!(m.result_type, ResultType::Coverage);
    }

    #[test]
    fn detect_clover_xml() {
        let content = r#"<?xml version="1.0"?><coverage generated="0" clover="4.0"><project timestamp="0"><metrics statements="10" coveredstatements="5" methods="2" coveredmethods="1"/></project></coverage>"#;
        let m = registry().detect(content, "clover.xml").unwrap();
        assert_eq!(m.parser_id, "clover");
        assert!(m.score >= 50);
        assert_eq!(m.result_type, ResultType::Coverage);
    }

    #[test]
    fn detect_returns_none_for_unknown() {
        let content = "hello world this is just plain text";
        assert!(registry().detect(content, "readme.txt").is_none());
    }

    #[test]
    fn detect_threshold_respected() {
        let unknown_content = "random binary data \x00\x01\x02";
        assert!(registry().detect(unknown_content, "data.bin").is_none());
    }

    #[test]
    fn detect_priority_tiebreaking() {
        // TRX in .xml file with TestRun and namespace should pick TRX over JUnit
        let content = r#"<?xml version="1.0"?><TestRun xmlns="http://microsoft.com/schemas/VisualStudio/TeamTest/2010"><Results><UnitTestResult testName="t" outcome="Passed"/></Results></TestRun>"#;
        let m = registry().detect(content, "report.xml").unwrap();
        assert_eq!(m.parser_id, "trx");
    }
}
