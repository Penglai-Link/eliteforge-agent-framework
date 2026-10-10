//! Process-only sampling: CPU fraction of one core (may exceed 1), RSS in bytes.
use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use std::time::Duration;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio_util::sync::CancellationToken;
pub(super) fn start(
    provider: SdkMeterProvider,
    stop: CancellationToken,
    sample_interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let meter = provider.meter("eliteforge.process");
        let cpu = meter
            .f64_gauge("process.cpu.utilization")
            .with_unit("1")
            .build();
        let memory = meter
            .u64_gauge("process.memory.usage")
            .with_unit("By")
            .build();
        let pid = Pid::from_u32(std::process::id());
        let mut system = System::new();
        let refresh = ProcessRefreshKind::nothing().with_cpu().with_memory();
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, refresh);
        let mut interval = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(1),
            sample_interval,
        );
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { _=stop.cancelled()=>break,_=interval.tick()=>{
                system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]),true,refresh);
                if let Some(process)=system.process(pid) {cpu.record(process.cpu_usage() as f64/100.,&[]);memory.record(process.memory(),&[]);}
            }}
        }
    })
}
