//! One physical owner's retained selection descriptors, independent of wire limits.
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::Error;

pub struct DescriptorBudget {
    maximum: usize,
    slots: Arc<Semaphore>,
}
pub(super) struct Reservation {
    // Release the slot before allowing a replacement physical owner.
    _permit: OwnedSemaphorePermit,
    _budget: Arc<DescriptorBudget>,
}
impl DescriptorBudget {
    /// The application supplies its process-wide selection FD allowance.
    /// Reuse this owner across native providers and replacement sessions.
    pub fn new(maximum: usize) -> Result<Arc<Self>, Error> {
        if maximum == 0 || maximum > Semaphore::MAX_PERMITS {
            return Err(Error::Capacity);
        }
        Ok(Arc::new(Self {
            maximum,
            slots: Arc::new(Semaphore::new(maximum)),
        }))
    }
    pub fn maximum(&self) -> usize {
        self.maximum
    }
    pub fn available(&self) -> usize {
        self.slots.available_permits()
    }
    pub(super) fn reserve(self: &Arc<Self>) -> Result<Reservation, Error> {
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Capacity)?;
        Ok(Reservation {
            _permit: permit,
            _budget: self.clone(),
        })
    }
}
