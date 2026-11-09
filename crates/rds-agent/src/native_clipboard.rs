//! Native clipboard preparation under the already verified desktop scope.
use rds_core::clipboard::{Capabilities, Side};
use rds_desktop::{
    DesktopSource, ServingClipboard,
    clipboard::{NativeLifetime, native::Policy, peer},
};
use rds_net::Connection;

pub(crate) struct Admission {
    pub connection: Connection,
    pub session: [u8; 16],
    pub display: u32,
    pub peer: Capabilities,
    pub original: NativeLifetime,
    pub permitted: bool,
}
pub(crate) struct Negotiated {
    pub mode: ServingClipboard,
    pub advertised: Option<Capabilities>,
}
pub(crate) fn prepare(
    source: Option<&dyn DesktopSource>,
    admission: Admission,
) -> anyhow::Result<Negotiated> {
    admission.peer.validate()?;
    let unavailable = || Negotiated {
        mode: ServingClipboard::Native(None),
        advertised: None,
    };
    if !admission.permitted {
        return Ok(unavailable());
    }
    let Some(source) = source else {
        return Ok(unavailable());
    };
    let provider = match source.native_clipboard(admission.display, admission.original.clone()) {
        Ok(provider) => provider,
        Err(error) => {
            tracing::debug!(%error, "prepared native clipboard unavailable");
            return Ok(unavailable());
        }
    };
    let caps = peer::capabilities(provider.as_ref())?;
    let accepted = caps.negotiate(&admission.peer)?;
    if accepted.receive.is_empty() && accepted.offer.is_empty() {
        drop(provider.close());
        return Ok(unavailable());
    }
    let prepared = peer::Prepared::new(peer::Setup {
        connection: admission.connection,
        desktop: admission.session,
        side: Side::Responder,
        provider,
        admitted: caps,
        peer: admission.peer,
        policy: Policy::shipped()?.peer,
        original: admission.original,
    })?;
    Ok(Negotiated {
        advertised: Some(prepared.capabilities().clone()),
        mode: ServingClipboard::Native(Some(Box::new(prepared))),
    })
}
