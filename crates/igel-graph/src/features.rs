use std::collections::{BTreeSet, HashMap};

#[derive(Debug, Default)]
pub struct FeatureResolver {
    resolved: HashMap<String, BTreeSet<String>>,
}

impl FeatureResolver {
    pub fn new() -> Self {
        Self {
            resolved: HashMap::new(),
        }
    }

    pub fn request_feature(&mut self, name: &str, feature: &str) {
        self.resolved
            .entry(name.to_string())
            .or_default()
            .insert(feature.to_string());
    }

    pub fn request_features(&mut self, name: &str, features: &[String]) {
        let entry = self.resolved.entry(name.to_string()).or_default();
        for feature in features {
            entry.insert(feature.clone());
        }
    }

    pub fn resolved_features(&self, name: &str) -> Vec<String> {
        self.resolved
            .get(name)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn apply_defaults(&mut self, name: &str, default_features: &[String]) {
        self.request_features(name, default_features);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feature_unification() {
        let mut resolver = FeatureResolver::new();

        resolver.request_features("crate_c", &["serde".to_string(), "json".to_string()]);

        resolver.request_features("crate_c", &["serde".to_string(), "xml".to_string()]);

        resolver.request_feature("crate_e", "async");

        let mut resolved_c = resolver.resolved_features("crate_c");
        resolved_c.sort();
        assert_eq!(resolved_c, vec!["json", "serde", "xml"]);

        let resolved_e = resolver.resolved_features("crate_e");
        assert_eq!(resolved_e, vec!["async"]);

        let resolved_f = resolver.resolved_features("crate_f");
        assert!(resolved_f.is_empty());
    }

    #[test]
    fn test_apply_defaults() {
        let mut resolver = FeatureResolver::new();
        resolver.apply_defaults(
            "crate_x",
            &["default_feat1".to_string(), "default_feat2".to_string()],
        );

        let mut resolved_x = resolver.resolved_features("crate_x");
        resolved_x.sort();
        assert_eq!(resolved_x, vec!["default_feat1", "default_feat2"]);
    }
}
