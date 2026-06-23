use sakala_agent_protocol::{AppliedRuntimeResources, RuntimeResourceLimits};

use crate::RuntimeError;

/// Node-level defaults and hard safety ceilings, not product plan policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceSafetyConfig {
    pub default_memory_mb: u64,
    pub max_memory_mb: u64,
    pub default_cpu_millis: u32,
    pub max_cpu_millis: u32,
    pub default_pids_limit: u32,
    pub max_pids_limit: u32,
}

impl Default for ResourceSafetyConfig {
    fn default() -> Self {
        Self {
            default_memory_mb: 256,
            max_memory_mb: 512,
            default_cpu_millis: 500,
            max_cpu_millis: 1_000,
            default_pids_limit: 128,
            max_pids_limit: 256,
        }
    }
}

impl ResourceSafetyConfig {
    pub fn resolve(
        self,
        requested: RuntimeResourceLimits,
    ) -> Result<AppliedRuntimeResources, RuntimeError> {
        validate_safety_config(self)?;

        let memory_mb = resolve_limit(
            "memory_mb",
            requested.memory_mb,
            self.default_memory_mb,
            self.max_memory_mb,
        )?;
        let cpu_millis = resolve_limit(
            "cpu_millis",
            requested.cpu_millis,
            self.default_cpu_millis,
            self.max_cpu_millis,
        )?;
        let pids_limit = resolve_limit(
            "pids_limit",
            requested.pids_limit,
            self.default_pids_limit,
            self.max_pids_limit,
        )?;

        Ok(AppliedRuntimeResources {
            memory_mb,
            cpu_millis,
            pids_limit,
        })
    }
}

fn resolve_limit<T>(
    name: &str,
    requested: Option<T>,
    default: T,
    maximum: T,
) -> Result<T, RuntimeError>
where
    T: Copy + Default + Ord + std::fmt::Display,
{
    let value = requested.unwrap_or(default);
    if value == T::default() {
        return Err(RuntimeError::InvalidCommand(format!(
            "requested {name} must be greater than zero"
        )));
    }
    if value > maximum {
        return Err(RuntimeError::InvalidCommand(format!(
            "requested {name} ({value}) exceeds this node's maximum ({maximum})"
        )));
    }
    Ok(value)
}

fn validate_safety_config(config: ResourceSafetyConfig) -> Result<(), RuntimeError> {
    for (name, default, maximum) in [
        ("memory_mb", config.default_memory_mb, config.max_memory_mb),
        (
            "cpu_millis",
            u64::from(config.default_cpu_millis),
            u64::from(config.max_cpu_millis),
        ),
        (
            "pids_limit",
            u64::from(config.default_pids_limit),
            u64::from(config.max_pids_limit),
        ),
    ] {
        if default == 0 || maximum == 0 || default > maximum {
            return Err(RuntimeError::Configuration(format!(
                "invalid resource safety config for {name}: default={default}, maximum={maximum}"
            )));
        }
    }
    Ok(())
}

#[must_use]
pub fn docker_cpu_value(cpu_millis: u32) -> String {
    let whole = cpu_millis / 1_000;
    let remainder = cpu_millis % 1_000;
    if remainder == 0 {
        return whole.to_string();
    }

    format!("{whole}.{remainder:03}")
        .trim_end_matches('0')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_values_use_node_defaults() {
        let applied = ResourceSafetyConfig::default()
            .resolve(RuntimeResourceLimits::default())
            .expect("defaults should be valid");

        assert_eq!(applied.memory_mb, 256);
        assert_eq!(applied.cpu_millis, 500);
        assert_eq!(applied.pids_limit, 128);
    }

    #[test]
    fn requests_above_node_maximum_are_rejected() {
        let error = ResourceSafetyConfig::default()
            .resolve(RuntimeResourceLimits {
                memory_mb: Some(1_024),
                ..RuntimeResourceLimits::default()
            })
            .expect_err("oversized request should fail");

        assert!(error.to_string().contains("exceeds this node's maximum"));
    }

    #[test]
    fn formats_milli_cpu_for_docker() {
        assert_eq!(docker_cpu_value(500), "0.5");
        assert_eq!(docker_cpu_value(1_000), "1");
        assert_eq!(docker_cpu_value(1_250), "1.25");
    }
}
