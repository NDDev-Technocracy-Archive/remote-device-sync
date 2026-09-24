//! Endpoint-owned policy tasks. Completed tasks release storage immediately.

use std::sync::Mutex;

use tokio_util::task::TaskTracker;

#[derive(Default)]
pub(super) struct Drivers {
    tasks: TaskTracker,
    // Serialize task admission with shutdown: TaskTracker::close alone does
    // not prohibit new tasks, so it cannot provide this lifecycle boundary.
    admission: Mutex<()>,
}

impl Drivers {
    pub fn spawn(
        &self,
        conn: &noq::Connection,
        seeds: Vec<noq::PathId>,
        metrics: crate::metrics::Registry,
    ) -> anyhow::Result<()> {
        let _guard = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        if self.tasks.is_closed() {
            conn.close(0u32.into(), b"endpoint closed");
            anyhow::bail!("endpoint closed before policy admission");
        }
        self.tasks.spawn(super::policy::connection_driver(
            conn.weak_handle(),
            conn.nat_traversal_updates(),
            conn.path_events(),
            seeds,
            metrics,
        ));
        Ok(())
    }

    pub fn close_admission(&self) {
        let _guard = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        self.tasks.close();
    }

    pub async fn wait(&self) {
        self.tasks.wait().await;
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }
}
