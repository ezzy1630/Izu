use std::collections::BTreeSet;

use crate::types::validate_argv;
use crate::{
    AdmissionBlock, JobSnapshot, ResourceBudget, ResourceRequest, ResourceUsage, RunRequest,
    RuntimeConfig, RuntimeError,
};

pub(crate) fn validate_request(
    request: &RunRequest,
    config: &RuntimeConfig,
) -> Result<(), RuntimeError> {
    let mut bytes = validate_argv(&request.argv, config.max_command_bytes, true)?;
    if request.env_overlay.len() > 4096 {
        return Err(RuntimeError::Invalid(
            "environment has more than 4096 entries".into(),
        ));
    }
    for (key, value) in &request.env_overlay {
        if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
            return Err(RuntimeError::Invalid(
                "invalid environment key or NUL value".into(),
            ));
        }
        bytes = bytes
            .checked_add(key.len())
            .and_then(|size| size.checked_add(value.len()))
            .and_then(|size| size.checked_add(2))
            .ok_or_else(|| RuntimeError::Invalid("environment size overflow".into()))?;
    }
    if bytes > config.max_command_bytes {
        return Err(RuntimeError::Invalid(
            "argv and environment exceed configured byte limit".into(),
        ));
    }
    if request.timeout_ms == 0 || request.timeout_ms > config.max_timeout_ms {
        return Err(RuntimeError::Invalid(
            "timeout must be positive and within configured maximum".into(),
        ));
    }
    let resources = &request.resources;
    if resources.cpu_slots == 0
        || resources.cpu_slots > config.budget.cpu_slots
        || resources.memory_bytes > config.budget.memory_bytes
        || resources.disk_bytes > config.budget.disk_bytes
    {
        return Err(RuntimeError::Capacity(
            "request exceeds the entire resource budget".into(),
        ));
    }
    if resources.ports.len() > 256
        || resources.ports.contains(&0)
        || resources
            .ports
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len()
            != resources.ports.len()
    {
        return Err(RuntimeError::Invalid(
            "ports must be distinct nonzero ports, at most 256".into(),
        ));
    }
    Ok(())
}

pub(crate) fn usage<'a>(
    jobs: impl Iterator<Item = &'a JobSnapshot>,
) -> Result<ResourceUsage, RuntimeError> {
    let mut usage = ResourceUsage::default();
    let mut ports = BTreeSet::new();
    for job in jobs.filter(|job| job.state.reserves_resources()) {
        usage.running_jobs = usage
            .running_jobs
            .checked_add(1)
            .ok_or_else(|| RuntimeError::Invalid("job count overflow".into()))?;
        usage.cpu_slots = usage
            .cpu_slots
            .checked_add(job.request.resources.cpu_slots)
            .ok_or_else(|| RuntimeError::Invalid("CPU reservation overflow".into()))?;
        usage.memory_bytes = usage
            .memory_bytes
            .checked_add(job.request.resources.memory_bytes)
            .ok_or_else(|| RuntimeError::Invalid("memory reservation overflow".into()))?;
        usage.disk_bytes = usage
            .disk_bytes
            .checked_add(job.request.resources.disk_bytes)
            .ok_or_else(|| RuntimeError::Invalid("disk reservation overflow".into()))?;
        if matches!(job.state, crate::JobState::OwnershipUnknown { .. }) {
            usage.unknown_writers = usage
                .unknown_writers
                .checked_add(1)
                .ok_or_else(|| RuntimeError::Invalid("unknown writer count overflow".into()))?;
        }
        ports.extend(job.request.resources.ports.iter().copied());
    }
    usage.ports = ports.into_iter().collect();
    Ok(usage)
}

pub(crate) fn admission(
    resources: &ResourceRequest,
    usage: &ResourceUsage,
    budget: &ResourceBudget,
) -> Option<AdmissionBlock> {
    // Unknown writers keep their original reservations. They cannot acquire new
    // resources by crashing, and cannot be treated as gone from a persisted PID.
    if usage.running_jobs >= budget.max_running_jobs {
        return Some(AdmissionBlock::RunningJobs);
    }
    if usage
        .cpu_slots
        .checked_add(resources.cpu_slots)
        .is_none_or(|sum| sum > budget.cpu_slots)
    {
        return Some(AdmissionBlock::CpuSlots);
    }
    if usage
        .memory_bytes
        .checked_add(resources.memory_bytes)
        .is_none_or(|sum| sum > budget.memory_bytes)
    {
        return Some(AdmissionBlock::MemoryReservation);
    }
    if usage
        .disk_bytes
        .checked_add(resources.disk_bytes)
        .is_none_or(|sum| sum > budget.disk_bytes)
    {
        return Some(AdmissionBlock::DiskReservation);
    }
    resources
        .ports
        .iter()
        .find(|port| usage.ports.contains(port))
        .map(|port| AdmissionBlock::PortClaim { port: *port })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cpu_memory_disk_and_ports_are_independent_admission_reasons() {
        let budget = ResourceBudget {
            max_running_jobs: 3,
            cpu_slots: 4,
            memory_bytes: 100,
            disk_bytes: 200,
            ..ResourceBudget::default()
        };
        let request = ResourceRequest {
            cpu_slots: 2,
            memory_bytes: 25,
            disk_bytes: 50,
            ports: vec![4300],
        };
        let mut current = ResourceUsage {
            cpu_slots: 3,
            ..ResourceUsage::default()
        };
        assert_eq!(
            admission(&request, &current, &budget),
            Some(AdmissionBlock::CpuSlots)
        );
        current.cpu_slots = 0;
        current.memory_bytes = 90;
        assert_eq!(
            admission(&request, &current, &budget),
            Some(AdmissionBlock::MemoryReservation)
        );
        current.memory_bytes = 0;
        current.disk_bytes = 180;
        assert_eq!(
            admission(&request, &current, &budget),
            Some(AdmissionBlock::DiskReservation)
        );
        current.disk_bytes = 0;
        current.ports = vec![4300];
        assert_eq!(
            admission(&request, &current, &budget),
            Some(AdmissionBlock::PortClaim { port: 4300 })
        );
    }
    #[test]
    fn reservation_arithmetic_cannot_wrap_to_admitted() {
        let request = ResourceRequest {
            cpu_slots: 1,
            memory_bytes: 1,
            disk_bytes: 0,
            ports: vec![],
        };
        let usage = ResourceUsage {
            memory_bytes: u64::MAX,
            ..ResourceUsage::default()
        };
        let budget = ResourceBudget {
            memory_bytes: u64::MAX,
            ..ResourceBudget::default()
        };
        assert_eq!(
            admission(&request, &usage, &budget),
            Some(AdmissionBlock::MemoryReservation)
        );
    }
}
