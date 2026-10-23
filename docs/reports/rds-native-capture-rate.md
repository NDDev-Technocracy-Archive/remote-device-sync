# Bound native capture by the explicit source limit

The portal copy cadence limited downstream pixel copies but its PipeWire format
offer omitted maxFramerate. Mutter's variable-framerate source could therefore
negotiate a 60 FPS maximum even with an explicit 30 FPS source cap, spending
software paint/readback work on frames the consumer would discard.

For an explicit cap, constrain both the ordinary rate range and maxFramerate.
Round the already validated integer-derived interval to its original rate and
retain the existing native upper bound of 60. With no explicit setting, preserve
the prior offer exactly. Pixel formats, geometry, encoder/viewer FPS, input and
transport are unchanged.

The tests deserialize the actual serialized SPA offer for every admitted 1–240
FPS setting, assert both ceilings, parse it using libspa's native video parser,
and retain the unconfigured/default behavior. This is source qualification;
it does not claim that an already-running portal renegotiates or that CPU usage
will fall by a fixed percentage. Native adoption needs a new portal preparation.

The [PipeWire format contract](https://docs.pipewire.org/devel/group__spa__param.html)
defines maxFramerate independently from framerate. Mutter's screen-cast source
uses the variable-rate form and publishes maxFramerate in its format offer.
