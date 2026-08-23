# Philips Hue

Hue bridges and everything paired to them — bulbs, motion sensors, dimmers, wall modules and
dials — over CLIP v2.

One package, several drivers. The bridge is a device in its own right: it holds the address and
the paired application key once, and everything behind it reads them from there — so a bridge
that moves to a new IP is edited in one place rather than six. They ship together because a
version skew between a bridge and its children is invisible until something stops working.

| Driver | Proxies | What it is |
| --- | --- | --- |
| `signify.hue.bridge` | `bridge` | The hub. Holds the connection, owns the event stream. |
| `signify.hue.light` | `light` | Any Hue light. What this one can do is answered per light. |
| `signify.hue.motion` | `sensor` ×3 | Motion, temperature and light level — one binding each. |
| `signify.hue.dimmer` | `button` ×4 | The four-button dimmer switch. |
| `signify.hue.tap_dial` | `button` ×5 | Four buttons and the dial. |
| `signify.hue.wall_switch` | `button` ×2 | The module behind an existing rocker. |
| `signify.hue.smart_button` | `button` | One button. |


### Taking the bridge's word for where things are

So each candidate carries the Hue room it is in, and core matches or creates that room at the
moment of adoption. It is a **suggestion**, not an instruction: nothing is created behind
anybody's back, the list is on screen when it happens, and the driver cannot rename or delete a
room. Rooms rather than zones, because a Hue room is exclusive — a device is in exactly one — and
"Downstairs" and "Evening" are both zones and neither is where a lamp *is*.

`behavior_instance` is read for the same reason and answers a second question. A dimmer paired
through the app is already wired to something; that is what pairing it did. Knowing it drives the
kitchen both names it — "controls Kitchen" beats "Hue dimmer switch 2" in a list you have to pick
from — and places it, since battery remotes are routinely in no Hue room at all and what a switch
drives is the best available answer to where it is.

### Bringing the rules over

The behaviors are also offered as Juno automations, and they arrive **switched off** and tagged
with the driver that read them. That is the whole of what makes it safe: an imported rule is this
driver's *interpretation* of somebody else's automation, and nothing should start behaving
differently in a house because a bridge was adopted. They land on the Automations page as
proposals with their origin written on them.

How much of an interpretation is worth being plain about. A Hue behavior says *that* a switch
drives a room; the per-button detail lives in a script whose shape is the script's own business and
changes between versions. So what is reconstructed is the layout every Hue remote has had since the
first one:

| Button | Rule |
| --- | --- |
| Top | `clicked` → room `all_lights_on` |
| Brighter | `clicked` **and** `repeating` → room `dim_up` |
| Dimmer | `clicked` **and** `repeating` → room `dim_down` |
| Bottom | `clicked` → room `all_lights_off` |

Brighter and dimmer take two triggers because that is one intention: Hue repeats while a button is
held, and `dim_up` is relative, so the same rule gives a step per click and a ramp per hold. A
motion sensor the bridge already lights a room with becomes one rule on `detected`.

A Tap Dial contributes only its ring — turn right brightens, turn left dims. Its four buttons
recall scenes, and a scene is the one thing on a Hue bridge with no Juno representation at all;
guessing a brightness for "Relax" would be inventing something nobody asked for.

Every imported rule crosses the same gate a hand-written one does. A trigger must name an event the
contract declares, a room command must exist, arguments are range-checked. Anything that does not
survive that is reported and dropped rather than bent until it fits.

The configuration inside a behavior is shaped by whichever script it is an instance of, and those
shapes are neither documented nor stable. Rather than walk a known path — which would work for
today's dimmer script and quietly stop working for the next one — the driver collects every
`{rid, rtype}` anywhere in the structure and keeps the ones that name a room. Deliberately
structure-blind, because the one thing every script has in common is that it refers to things by
resource id.

## Building

```bash
cargo build --release
```

Releases are built by [`junohouse/driver-ci`](https://github.com/junohouse/driver-ci): push to
`main` for a beta, tag `v1.2.0` for a release. To work on this against a local core checkout,
uncomment the `[patch]` block in `Cargo.toml`.
