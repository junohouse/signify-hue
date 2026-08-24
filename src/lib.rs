//! Philips Hue bulbs over the bridge's CLIP v2 API, and the setup flow that finds them.
//!
//! # Setting one up
//!
//! The driver owns the whole conversation, because only it knows what a Hue bridge wants:
//!
//! ```text
//!   1. ask for the bridge address (or check the one offered)
//!   2. GET  /api/config                — is this really a bridge?
//!   3. "press the link button"
//!   4. POST /api                       — refused until it is pressed, so poll
//!   5. GET  /clip/v2/resource/light    — the real bulbs, read off the hardware
//!   6. offer them; whatever is picked is confirmed and adopted
//! ```
//!
//! Everything is CLIP v2 over HTTPS, including setup. Mixing v1 and v2 is a trap: v1 numbers
//! its lights `1`, `2`, `3` while v2 identifies them by UUID, so a flow that pairs with one
//! and commands with the other produces device ids that 404. The bridge's certificate is
//! self-signed — see the note on the controller's TLS layer.
//!
//! Core performs every request and renders every screen without knowing any of this.
//!
//! One device per bulb rather than one per bridge: five bulbs are five devices sharing one
//! loaded module, and each can live in a different room. The bridge address and application
//! key are per-device properties, so two bridges in one house need no special handling.

use driver_sdk::*;
use std::collections::BTreeMap;
use driver_sdk::{Value, json};

mod button;
mod catalog;
mod scene;
mod sensor;

#[derive(Default)]
pub struct HueBulb;

/// What a given device behind this bridge actually is.
///
/// One loaded module answers for all of them. Core tells `discover` and `setup` which manifest they
/// are running as, but `on_bind`, `on_command` and `on_event` are not given a driver id — so the
/// runtime half has to work it out, and the honest signal is which properties the installer's
/// adoption actually set. A bulb has a `Light id` and a keypad does not; nothing else needs asking.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The hub. Holds the address and key, owns the event stream, and is not in any room.
    Bridge,
    Bulb,
    Motion,
    /// A keypad, remote, wall module or dial.
    Control,
}

impl Role {
    fn of(inst: &Instance) -> Role {
        let has = |property: &str| {
            inst.property(property)
                .as_str()
                .is_some_and(|s| !s.is_empty())
        };
        if has("Light id") {
            Role::Bulb
        } else if has("Motion id") {
            Role::Motion
        } else if has("Button 1 id") || has("Rotary id") {
            Role::Control
        } else {
            Role::Bridge
        }
    }

    /// Every binding this device has, so all of them can be brought online at bind rather than
    /// binding 1 alone. A multi-sensor whose temperature never came online reads as a broken probe.
    fn bindings(self, inst: &Instance) -> Vec<LocalId> {
        let set = |property: &str| {
            inst.property(property)
                .as_str()
                .is_some_and(|s| !s.is_empty())
        };
        match self {
            Role::Bridge | Role::Bulb => vec![1],
            Role::Motion => [
                ("Motion id", 1),
                ("Temperature id", 2),
                ("Light level id", 3),
            ]
            .iter()
            .filter(|(p, _)| set(p))
            .map(|(_, id)| *id)
            .collect(),
            Role::Control => [
                ("Button 1 id", 1),
                ("Button 2 id", 2),
                ("Button 3 id", 3),
                ("Button 4 id", 4),
                ("Rotary id", 5),
            ]
            .iter()
            .filter(|(p, _)| set(p))
            .map(|(_, id)| *id)
            .collect(),
        }
    }

    /// The properties holding this role's CLIP v2 resource ids.
    ///
    /// Separate from [`Role::bindings`] because they answer different questions — that one maps
    /// properties to binding numbers, this one only needs the property names — and because a
    /// device's identity on the bridge is not the same list as the bindings it drives. Keeping
    /// them apart means a role that gains a service does not silently gain a way to be deleted.
    fn id_properties(self) -> &'static [&'static str] {
        match self {
            Role::Bridge => &[],
            Role::Bulb => &["Light id"],
            Role::Motion => &["Motion id", "Temperature id", "Light level id"],
            Role::Control => &[
                "Button 1 id",
                "Button 2 id",
                "Button 3 id",
                "Button 4 id",
                "Rotary id",
            ],
        }
    }

    /// Whether a CLIP v2 resource id is one this device answers to.
    ///
    /// Any of them is enough. A motion sensor is one piece of hardware with three services on
    /// it, and unpairing it deletes all three — waiting for every id before believing it would
    /// leave the device half-removed if the bridge only names two.
    fn owns(self, inst: &Instance, rid: &str) -> bool {
        self.id_properties()
            .iter()
            .filter_map(|p| inst.property(p).as_str().map(str::to_string))
            .any(|mine| !mine.is_empty() && mine == rid)
    }
}

/// A power command is deliberately only a power command. In particular, `on` must not include a
/// dimming value: Hue then restores the light's own last level, including changes made outside
/// Juno, instead of Juno accidentally replacing it with a cached/default 100%.
fn power_body(on: bool, ramp_ms: Option<u64>) -> Value {
    let mut body = json!({ "on": { "on": on } });
    if let Some(ms) = ramp_ms {
        body["dynamics"] = json!({ "duration": ms.min(6_553_000) });
    }
    body
}

/// Hue takes brightness as a percentage but treats 0 as "dimmest on", not off — so a level of
/// 0 has to become `on: false` or the bulb sits at 1% instead of going out.
fn level_body(level: u8, ramp_ms: Option<u64>) -> Value {
    let mut body = power_body(level > 0, ramp_ms);
    if level > 0 {
        body["dimming"] = json!({ "brightness": level as f64 });
    }
    body
}

/// CIE xy from hue/saturation at full value. The bridge wants a gamut point, not HSV.
/// The inverse of [`hs_to_xy`], for reading a bulb's color back off the bridge.
///
/// The bridge answers in CIE xy and the `light` contract is hue and saturation, so without this
/// the raw chromaticity went straight into those fields: a lamp reported `hue = 0.2858`,
/// `sat = 0.3083` where the contract means degrees and percent. Every surface reading color —
/// the tile, the scene editor capturing what a room looks like now — got a number a hundredth
/// of the size it expected and drew the wrong color.
///
/// Luminance is fixed at 1.0 on the way back: xy carries no brightness, which is `level`'s job,
/// and reconstructing one here would fight it.
fn xy_to_hs(x: f64, y: f64) -> (f64, f64) {
    if y <= 0.0 {
        return (0.0, 0.0);
    }
    // xyY -> XYZ at full luminance, then the inverse of the Wide RGB D65 matrix above.
    let (big_x, big_y, big_z) = (x / y, 1.0, (1.0 - x - y) / y);
    // The exact inverse of the matrix in `hs_to_xy`, computed from it rather than copied from
    // the widely-quoted Philips one — those two are not each other, and the mismatch put full
    // blue back as 257 degrees instead of 240.
    let r = big_x * 1.611_757 + big_y * -0.202_805 + big_z * -0.302_298;
    let g = big_x * -0.509_057 + big_y * 1.411_914 + big_z * 0.066_070;
    let b = big_x * 0.026_086 + big_y * -0.072_353 + big_z * 0.962_086;
    // Scale while still linear, then gamma-encode. The other order runs the curve over values
    // above 1 — a saturated blue leaves this matrix at b ≈ 46 — and the curve is not linear, so
    // the ratios between the channels come out changed and the hue with them.
    let max = r.max(g).max(b);
    if max <= 0.0 {
        return (0.0, 0.0);
    }
    let srgb = |u: f64| {
        let u = (u / max).clamp(0.0, 1.0);
        if u <= 0.003_130_8 {
            12.92 * u
        } else {
            1.055 * u.powf(1.0 / 2.4) - 0.055
        }
    };
    let (r, g, b) = (srgb(r), srgb(g), srgb(b));
    let min = r.min(g).min(b);
    let delta = 1.0 - min;
    if delta <= f64::EPSILON {
        return (0.0, 0.0); // white: no hue to report
    }
    let hue = if r >= g && r >= b {
        60.0 * (((g - b) / delta).rem_euclid(6.0))
    } else if g >= b {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    (hue.rem_euclid(360.0), (delta * 100.0).clamp(0.0, 100.0))
}

fn hs_to_xy(hue_deg: f64, sat_pct: f64) -> (f64, f64) {
    let h = hue_deg.rem_euclid(360.0) / 60.0;
    let s = (sat_pct / 100.0).clamp(0.0, 1.0);
    let c = s;
    let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = 1.0 - c;
    let (r, g, b) = (r + m, g + m, b + m);

    // sRGB -> linear -> CIE XYZ (Wide RGB D65, the transform Signify documents)
    let lin = |u: f64| {
        if u > 0.04045 {
            ((u + 0.055) / 1.055).powf(2.4)
        } else {
            u / 12.92
        }
    };
    let (r, g, b) = (lin(r), lin(g), lin(b));
    let big_x = r * 0.649_926 + g * 0.103_455 + b * 0.197_109;
    let big_y = r * 0.234_327 + g * 0.743_075 + b * 0.022_598;
    let big_z = g * 0.053_077 + b * 1.035_763;
    let sum = big_x + big_y + big_z;
    if sum <= 0.0 {
        return (0.3127, 0.3290); // D65 white; a black point has no chromaticity
    }
    (big_x / sum, big_y / sum)
}

/// The collections the bridge reads once at start, so a controller that has just come up knows
/// where the house stands without waiting for something to change.
///
/// `button` is deliberately not among them, and the omission is the interesting part. A button
/// resource carries its *last* event, which on a bridge that has been up for a week is a press from
/// last Tuesday — reading it at start would report that press as if it had just happened, and every
/// rule attached to that button would fire on a controller restart. Lights, motion, temperature and
/// battery are states and can be read; a press is an event and can only be listened for.
const AT_START: &[&str] = &[
    "light",
    "motion",
    "temperature",
    "light_level",
    "device_power",
    // Zones are inventory, not Juno-owned configuration. Reading them lets a logical group use
    // an exact existing match without ever renaming it or changing its members.
    "zone",
    // Native scenes can belong to rooms or zones. These inventories are read once so publishing
    // can select an exact existing scope without modifying it, and borrowed scenes can be
    // validated before recall.
    "room",
    "scene",
];

const HUE_BRIDGE_ID: &str = "hue_bridge_id";
const HUE_ZONES: &str = "hue_zones";
const HUE_GROUP_LINKS: &str = "hue_group_links";
const HUE_GROUP_PENDING: &str = "hue_group_pending";
const HUE_GROUP_PROBLEM: &str = "hue_group_problem";

/// A path, not a URL: core resolves the address, the port and the scheme from the project, and
/// a bulb inherits all three from the bridge exactly as it inherits `Bridge address`.
///
/// The address is still read, but only to answer whether there is one — a request for a bridge
/// nobody has finished setting up should not be built at all.
fn bridge_http(inst: &Instance, method: &str, path: &str, body: Option<Value>) -> Option<HostCall> {
    let key = inst.property("Application key").as_str().unwrap_or("").to_string();
    if inst.property("Bridge address").as_str()?.is_empty() {
        return None;
    }
    let mut request = HttpRequest::new(method, path).header("hue-application-key", key);
    if let Some(body) = body {
        request = request.json(body.to_string());
    }
    Some(HostCall::Http(request))
}

fn group_key(group: DeviceId) -> String {
    group.to_string()
}

fn group_light_ids(request: &GroupRequest) -> Result<Vec<String>, String> {
    let mut ids = Vec::with_capacity(request.members.len());
    for member in &request.members {
        let Some(id) = member.instance.property("Light id").as_str() else {
            return Err(format!("device {} is not a Hue light", member.device));
        };
        if id.is_empty() {
            return Err(format!("device {} has no Hue light id", member.device));
        }
        ids.push(id.to_string());
    }
    ids.sort();
    ids.dedup();
    if ids.len() != request.members.len() {
        return Err("the group contains the same Hue light more than once".into());
    }
    if ids.is_empty() {
        return Err("the group has no Hue lights".into());
    }
    Ok(ids)
}

fn zone_light_ids(zone: &Value) -> Vec<String> {
    let mut ids: Vec<String> = zone
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|child| child.get("rtype").and_then(Value::as_str) == Some("light"))
        .filter_map(|child| child.get("rid").and_then(Value::as_str).map(str::to_string))
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

fn zone_grouped_light(zone: &Value) -> Option<&str> {
    zone.get("services")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|service| service.get("rtype").and_then(Value::as_str) == Some("grouped_light"))
        .and_then(|service| service.get("rid"))
        .and_then(Value::as_str)
}

fn zone_name(zone: &Value) -> &str {
    zone.pointer("/metadata/name")
        .and_then(Value::as_str)
        .unwrap_or("Unnamed Hue zone")
}

fn cached_zones(inst: &Instance) -> Vec<&Value> {
    inst.scratch
        .get(HUE_ZONES)
        .and_then(Value::as_array)
        .map(|zones| zones.iter().collect())
        .unwrap_or_default()
}

fn cached_zone<'a>(inst: &'a Instance, id: &str) -> Option<&'a Value> {
    cached_zones(inst)
        .into_iter()
        .find(|zone| zone.get("id").and_then(Value::as_str) == Some(id))
}

fn group_link(inst: &Instance, group: DeviceId) -> Option<Value> {
    inst.scratch
        .get(HUE_GROUP_LINKS)
        .and_then(Value::as_object)
        .and_then(|links| links.get(&group_key(group)))
        .cloned()
}

fn set_group_link(inst: &mut Instance, group: DeviceId, link: Value) {
    if !inst
        .scratch
        .get(HUE_GROUP_LINKS)
        .is_some_and(Value::is_object)
    {
        inst.scratch.insert(HUE_GROUP_LINKS.into(), json!({}));
    }
    if let Some(links) = inst
        .scratch
        .get_mut(HUE_GROUP_LINKS)
        .and_then(Value::as_object_mut)
    {
        links.insert(group_key(group), link);
    }
}

fn remove_group_link(inst: &mut Instance, group: DeviceId) {
    if let Some(links) = inst
        .scratch
        .get_mut(HUE_GROUP_LINKS)
        .and_then(Value::as_object_mut)
    {
        links.remove(&group_key(group));
    }
}

fn bridge_id(inst: &Instance) -> Option<&str> {
    inst.scratch.get(HUE_BRIDGE_ID).and_then(Value::as_str)
}

fn owned_zone_name(group: DeviceId, name: &str) -> (String, String) {
    // The suffix is visible evidence when somebody inspects the Hue app, but never ownership
    // authority by itself. Only the bridge-scoped local record below grants mutation rights.
    let token = format!("[Juno {group:08X}]");
    let room = 32usize.saturating_sub(token.chars().count() + 1);
    let prefix: String = name.chars().take(room).collect();
    (format!("{prefix} {token}"), token)
}

fn zone_children(light_ids: &[String]) -> Value {
    Value::Array(
        light_ids
            .iter()
            .map(|id| json!({ "rid": id, "rtype": "light" }))
            .collect(),
    )
}

fn group_status(inst: &Instance, request: &GroupRequest) -> Value {
    let desired = group_light_ids(request).unwrap_or_default();
    let link = group_link(inst, request.group);
    let linked_zone = link
        .as_ref()
        .and_then(|l| l.get("zone").and_then(Value::as_str))
        .and_then(|id| cached_zone(inst, id));

    let zones: Vec<Value> = cached_zones(inst)
        .into_iter()
        .filter_map(|zone| {
            let id = zone.get("id").and_then(Value::as_str)?;
            let members = zone_light_ids(zone);
            Some(json!({
                "resource": id,
                "name": zone_name(zone),
                "members": members,
                "exact_match": members == desired,
                "controllable": zone_grouped_light(zone).is_some(),
                // A name that looks like ours is intentionally not treated as ownership.
                "juno_owned": link.as_ref().is_some_and(|l|
                    l.get("ownership").and_then(Value::as_str) == Some("juno")
                        && l.get("zone").and_then(Value::as_str) == Some(id)),
            }))
        })
        .collect();

    let linked = link.map(|mut link| {
        let valid = linked_zone.is_some_and(|zone| {
            zone_light_ids(zone) == desired
                && zone_grouped_light(zone) == link.get("grouped_light").and_then(Value::as_str)
                && bridge_id(inst) == link.get("bridge_id").and_then(Value::as_str)
        });
        if let Some(object) = link.as_object_mut() {
            object.remove("token");
            object.remove("light_ids");
            object.insert("valid".into(), json!(valid));
        }
        link
    });

    json!({
        "bridge_ready": bridge_id(inst).is_some(),
        "linked": linked,
        "zones": zones,
        "problem": inst.scratch.get(HUE_GROUP_PROBLEM).cloned().unwrap_or(Value::Null),
    })
}

fn refused(problem: impl Into<String>, status: Value) -> GroupResponse {
    GroupResponse {
        disposition: GroupDisposition::Refused,
        problem: Some(problem.into()),
        status,
        ..Default::default()
    }
}

fn member_is_on(member: &GroupMember) -> bool {
    member
        .state
        .get("level")
        .and_then(Value::as_u64)
        .is_some_and(|level| level > 0)
        || member
            .instance
            .scratch
            .get("on")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

fn member_last_level(member: &GroupMember) -> u8 {
    member
        .instance
        .scratch
        .get("level")
        .and_then(Value::as_u64)
        .or_else(|| member.state.get("level").and_then(Value::as_u64))
        .unwrap_or(100)
        .clamp(1, 100) as u8
}

fn grouped_command(
    request: &GroupRequest,
    command: &str,
    args: &Args,
) -> Result<(Value, Vec<GroupMemberCalls>), String> {
    let ramp = args.get("ramp_ms").and_then(Value::as_u64);
    let any_on = request.members.iter().any(member_is_on);
    let representative_level = request
        .members
        .iter()
        .map(member_last_level)
        .max()
        .unwrap_or(100);

    let (body, shared_level, restore_each) = match command {
        "on" => (power_body(true, ramp), None, true),
        "off" => (power_body(false, ramp), Some(0), false),
        "toggle" if any_on => (power_body(false, ramp), Some(0), false),
        "toggle" => (power_body(true, ramp), None, true),
        "set_level" => {
            let level = args
                .get("level")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(100) as u8;
            (level_body(level, ramp), Some(level), false)
        }
        "set_cct" => {
            let kelvin = args.get("kelvin").and_then(Value::as_u64).unwrap_or(2700);
            let mirek = (1_000_000 / kelvin.max(1)).clamp(153, 500);
            (
                json!({ "color_temperature": { "mirek": mirek } }),
                None,
                false,
            )
        }
        "set_color" => {
            let hue = args.get("hue").and_then(Value::as_f64).unwrap_or(0.0);
            let sat = args.get("sat").and_then(Value::as_f64).unwrap_or(0.0);
            let (x, y) = hs_to_xy(hue, sat);
            (
                json!({ "color": { "xy": { "x": x, "y": y } } }),
                None,
                false,
            )
        }
        "ramp_start" | "ramp_stop" => {
            let up = args.get("direction").and_then(Value::as_str) == Some("up");
            let target = if command == "ramp_stop" {
                representative_level
            } else if up {
                100
            } else {
                1
            };
            (level_body(target, Some(4000)), Some(target), false)
        }
        other => return Err(format!("Hue grouped control does not support `{other}`")),
    };

    let members = request
        .members
        .iter()
        .map(|member| {
            let mut scratch = member.instance.scratch.clone();
            let mut calls = Vec::new();
            let level = if restore_each {
                Some(member_last_level(member))
            } else {
                shared_level
            };
            if let Some(level) = level {
                if level > 0 {
                    scratch.insert("level".into(), json!(level));
                }
                scratch.insert("on".into(), json!(level > 0));
                calls.extend(HueBulb::optimiztic(level));
            }
            if command == "set_cct"
                && let Some(kelvin) = args.get("kelvin").and_then(Value::as_u64)
            {
                let mut changed = Args::new();
                changed.insert("kelvin".into(), json!(kelvin));
                calls.push(HostCall::notify(member.proxy, "cct_changed", changed));
            }
            if command == "set_color" {
                let mut changed = Args::new();
                changed.insert("hue".into(), args.get("hue").cloned().unwrap_or(json!(0.0)));
                changed.insert("sat".into(), args.get("sat").cloned().unwrap_or(json!(0.0)));
                calls.push(HostCall::notify(member.proxy, "color_changed", changed));
            }
            GroupMemberCalls {
                device: member.device,
                calls,
                scratch: Some(scratch),
            }
        })
        .collect();
    Ok((body, members))
}

fn validated_grouped_light(inst: &Instance, request: &GroupRequest) -> Result<String, String> {
    let desired = group_light_ids(request)?;
    let link = group_link(inst, request.group)
        .ok_or_else(|| "this Juno group is not linked to a Hue zone".to_string())?;
    let current_bridge =
        bridge_id(inst).ok_or_else(|| "the Hue bridge identity has not loaded yet".to_string())?;
    if link.get("bridge_id").and_then(Value::as_str) != Some(current_bridge) {
        return Err("the saved zone belongs to a different Hue bridge".into());
    }
    let zone_id = link
        .get("zone")
        .and_then(Value::as_str)
        .ok_or_else(|| "the saved Hue zone link is incomplete".to_string())?;
    let zone = cached_zone(inst, zone_id)
        .ok_or_else(|| "the linked Hue zone no longer exists".to_string())?;
    if zone_light_ids(zone) != desired {
        return Err("the linked Hue zone membership changed; using individual lights".into());
    }
    let grouped = zone_grouped_light(zone)
        .ok_or_else(|| "the linked Hue zone has no grouped-light service".to_string())?;
    if link.get("grouped_light").and_then(Value::as_str) != Some(grouped) {
        return Err("the linked Hue zone service changed; using individual lights".into());
    }
    Ok(grouped.to_string())
}

fn cache_zone_inventory(inst: &mut Instance, data: &[Value]) {
    let zones: Vec<Value> = data
        .iter()
        .filter(|resource| {
            resource.get("type").and_then(Value::as_str) == Some("zone")
                || resource.get("rtype").and_then(Value::as_str) == Some("zone")
        })
        .cloned()
        .collect();
    inst.scratch.insert(HUE_ZONES.into(), Value::Array(zones));
    finish_group_pending(inst);
}

fn finish_group_pending(inst: &mut Instance) {
    let Some(pending) = inst.scratch.get(HUE_GROUP_PENDING).cloned() else {
        return;
    };
    let Some(zone_id) = pending.get("zone").and_then(Value::as_str) else {
        return; // a create is still waiting for Hue to return the new resource id
    };
    let Some(zone) = cached_zone(inst, zone_id).cloned() else {
        inst.scratch.insert(
            HUE_GROUP_PROBLEM.into(),
            json!("Hue did not return the zone after writing it"),
        );
        inst.scratch.remove(HUE_GROUP_PENDING);
        return;
    };
    let expected: Vec<String> = pending
        .get("light_ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|id| id.as_str().map(str::to_string))
        .collect();
    let Some(grouped_light) = zone_grouped_light(&zone).map(str::to_string) else {
        inst.scratch.insert(
            HUE_GROUP_PROBLEM.into(),
            json!("Hue created a zone without grouped-light control"),
        );
        inst.scratch.remove(HUE_GROUP_PENDING);
        return;
    };
    let token = pending.get("token").and_then(Value::as_str).unwrap_or("");
    if zone_light_ids(&zone) != expected || !zone_name(&zone).contains(token) {
        inst.scratch.insert(
            HUE_GROUP_PROBLEM.into(),
            json!("Hue returned a zone that did not match Juno's requested name and members"),
        );
        inst.scratch.remove(HUE_GROUP_PENDING);
        return;
    }
    let Some(group) = pending.get("group").and_then(Value::as_u64) else {
        inst.scratch.remove(HUE_GROUP_PENDING);
        return;
    };
    set_group_link(
        inst,
        group as DeviceId,
        json!({
            "ownership": "juno",
            "bridge_id": pending.get("bridge_id").cloned().unwrap_or(Value::Null),
            "zone": zone_id,
            "grouped_light": grouped_light,
            "light_ids": expected,
            "token": token,
            "name": zone_name(&zone),
        }),
    );
    inst.scratch.remove(HUE_GROUP_PENDING);
    inst.scratch.remove(HUE_GROUP_PROBLEM);
}

impl HueBulb {
    fn request(inst: &Instance, body: Value) -> Option<HostCall> {
        let bridge = inst.property("Bridge address").as_str()?.to_string();
        let key = inst.property("Application key").as_str().unwrap_or("");
        let id = inst.property("Light id").as_str()?.to_string();
        if bridge.is_empty() || id.is_empty() {
            return None;
        }
        Some(HostCall::Http(
            HttpRequest::new("PUT", format!("/clip/v2/resource/light/{id}"))
            .header("hue-application-key", key)
            .json(body.to_string()),
        ))
    }

    /// One frame of the bridge's event stream, as it concerns this bulb.
    ///
    /// The frame is the whole house — a scene recall names eight lights in one push — and every
    /// bulb behind the bridge is handed the same text. Keeping only what names us is the rule
    /// that makes one connection serve twenty-four devices: without it, one light changing at a
    /// wall switch would move all of them.
    ///
    /// ```text
    /// [{"type":"update","data":[{"id":"<rid>","type":"light","dimming":{"brightness":42}}]}]
    /// ```
    ///
    /// The envelope's own `type` is `update`, `add` or `delete`, and the difference matters:
    /// `add` is somebody pairing a bulb in the Hue app, and its resources belong to nothing in
    /// the project. Read as an update it matches no adopted device and is silently dropped, so
    /// a new bulb stays invisible to Juno until a person happens to run setup again.
    fn on_stream(&self, inst: &mut Instance, args: &Args) -> Vec<HostCall> {
        let Some(text) = args.get("data").and_then(Value::as_str) else {
            return Vec::new();
        };
        let Ok(frame) = serde_json::from_str::<Value>(text) else {
            return Vec::new(); // a keep-alive or a partial line core has not finished
        };

        let mut out = Vec::new();
        let mut zone_changed = false;
        let mut scene_changed = false;
        let mut room_changed = false;
        for update in frame.as_array().into_iter().flatten() {
            let kind = update
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("update");
            for resource in update
                .get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                zone_changed |= resource.get("type").and_then(Value::as_str) == Some("zone")
                    || resource.get("rtype").and_then(Value::as_str) == Some("zone");
                scene_changed |= resource.get("type").and_then(Value::as_str) == Some("scene")
                    || resource.get("rtype").and_then(Value::as_str) == Some("scene");
                room_changed |= resource.get("type").and_then(Value::as_str) == Some("room")
                    || resource.get("rtype").and_then(Value::as_str) == Some("room");
                out.extend(Self::mine(inst, kind, resource));
            }
        }
        // Zone update events may be partial. Re-reading the small collection keeps membership
        // validation authoritative and ensures an external Hue edit disables native dispatch
        // before the next command instead of trying to merge a partial patch incorrectly.
        if Role::of(inst) == Role::Bridge
            && zone_changed
            && let Some(call) = bridge_http(inst, "GET", "/clip/v2/resource/zone", None)
        {
            out.push(call);
        }
        if Role::of(inst) == Role::Bridge
            && scene_changed
            && let Some(call) = bridge_http(inst, "GET", "/clip/v2/resource/scene", None)
        {
            out.push(call);
        }
        if Role::of(inst) == Role::Bridge
            && room_changed
            && let Some(call) = bridge_http(inst, "GET", "/clip/v2/resource/room", None)
        {
            out.push(call);
        }
        out
    }

    /// One CLIP v2 resource, handed to whichever half of this driver knows what it means.
    ///
    /// Empty for everything that is not about this device, which is most of every frame — the
    /// bridge publishes the whole house on one connection and core hands each frame to all of it.
    /// The dispatch is on the device's role rather than on the resource's `type`, because the
    /// question being answered is "is this mine", and only the device knows which ids are its own.
    fn mine(inst: &mut Instance, kind: &str, resource: &Value) -> Vec<HostCall> {
        // Nothing already in the project can own a resource that has just been created, so an
        // `add` never reaches the roles below — only the bridge, which offers it to core.
        if kind == "add" {
            return match Role::of(inst) {
                Role::Bridge => Self::offer(resource),
                _ => Vec::new(),
            };
        }
        // The opposite: only the device that answers to the deleted id has anything to say, and
        // what it says is that it no longer exists. The bridge stays out of it — it hears every
        // deletion in the house and has no business removing devices it does not own.
        //
        // A delete frame carries the id and the type and nothing else, so there is no state to
        // report and the reporting paths below would find nothing in it anyway.
        if kind == "delete" {
            let role = Role::of(inst);
            let Some(rid) = resource.get("id").and_then(Value::as_str) else {
                return Vec::new();
            };
            return match role.owns(inst, rid) {
                true => vec![HostCall::gone("unpaired at the Hue bridge")],
                false => Vec::new(),
            };
        }
        match Role::of(inst) {
            Role::Bulb => {
                let mine = inst.property("Light id").as_str().map(str::to_string);
                match (mine, resource.get("id").and_then(Value::as_str)) {
                    (Some(mine), Some(id)) if mine == id => Self::report(inst, resource),
                    _ => Vec::new(),
                }
            }
            Role::Motion => sensor::report(inst, resource),
            Role::Control => button::report(inst, resource),
            // The bridge hearing its own stream. Everything on it belongs to something behind it.
            Role::Bridge => Vec::new(),
        }
    }

    /// A resource the bridge has just gained, told to core so it can ask whether the house wants it.
    ///
    /// Only `device` resources, and that is the whole trick. Pairing one bulb creates five or six
    /// resources — the `device`, its `light`, its `zigbee_connectivity`, an entertainment segment,
    /// a vendor-specific blob — and every one of them arrives as its own `add`. Offering all of
    /// them would put six prompts on somebody's phone for one bulb they just screwed in. The
    /// `device` is the one that stands for the physical thing.
    fn offer(resource: &Value) -> Vec<HostCall> {
        if resource.get("type").and_then(Value::as_str) != Some("device") {
            return Vec::new();
        }
        let Some(id) = resource.get("id").and_then(Value::as_str) else {
            return Vec::new();
        };
        let product = resource.get("product_data");
        // `metadata.name` is what the person typed in the Hue app, and is missing when they
        // accepted the default. The product name is Signify's own ("Hue color lamp") and is
        // always there, which makes it the better fallback than printing a UUID at somebody.
        let name = resource
            .get("metadata")
            .and_then(|m| m.get("name"))
            .and_then(Value::as_str)
            .or_else(|| product?.get("product_name")?.as_str())
            .unwrap_or(id);
        let kind = product
            .and_then(|p| p.get("product_archetype"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let args = [
            ("id", json!(id)),
            ("name", json!(name)),
            ("kind", json!(kind)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        vec![HostCall::notify(1, "device_added", args)]
    }

    /// Turn a light resource — from a poll or from the stream, they are the same shape — into
    /// what changed. Shared so the two paths cannot drift into disagreeing about a bulb.
    ///
    /// An event carries only the fields that moved: brightness alone when someone drags a
    /// slider, `on` alone when they flick a switch. So a missing field means "unchanged", and
    /// the remembered value fills it in — reading it as zero would darken the bulb on screen
    /// every time somebody changed its color.
    fn report(inst: &mut Instance, light: &Value) -> Vec<HostCall> {
        // The startup light collection is also the requested one-time effects_v2 re-scan. Each
        // adopted bulb keeps only its own advertised values for later scene validation.
        scene::cache_effects_v2(inst, light);
        let mut out = Vec::new();
        let known_level = inst
            .scratch
            .get("level")
            .and_then(Value::as_u64)
            .unwrap_or(100);
        let known_on = inst.scratch.get("on").and_then(Value::as_bool);

        let on = light
            .pointer("/on/on")
            .and_then(Value::as_bool)
            .or(known_on);
        let brightness = light
            .pointer("/dimming/brightness")
            .and_then(Value::as_f64)
            .map(|b| b.round().clamp(1.0, 100.0) as u64);

        // An off Hue resource still carries the level it will restore on the next plain `on`.
        // Keep it even though the effective level reported to Juno below is zero.
        if let Some(level) = brightness {
            inst.scratch.insert("level".into(), json!(level));
        }

        if let Some(on) = on {
            // Said outright rather than left to be inferred from the level. A Hue bulb keeps the
            // brightness it will return to while it is off, so the two are genuinely separate
            // here and core cannot work one out from the other: reporting brightness 47 for a
            // lamp somebody just switched off used to turn it back on on screen.
            inst.scratch.insert("on".into(), json!(on));
            let mut a = Args::new();
            a.insert("on".into(), json!(on));
            out.push(HostCall::notify(1, "power_changed", a));
        }
        if let Some(level) = brightness.or_else(|| on.map(|_| known_level)) {
            // The brightness the bulb holds, which is what it will come back at. Power is the
            // other notification's business now, so this no longer reports zero to mean off.
            inst.scratch.insert("level".into(), json!(level));
            let mut a = Args::new();
            a.insert("level".into(), json!(level));
            out.push(HostCall::notify(1, "level_changed", a));
        }

        if let Some(mirek) = light
            .pointer("/color_temperature/mirek")
            .and_then(Value::as_u64)
            .filter(|m| *m > 0)
        {
            let mut a = Args::new();
            a.insert("kelvin".into(), json!(1_000_000 / mirek));
            out.push(HostCall::notify(1, "cct_changed", a));
        }

        if let (Some(x), Some(y)) = (
            light.pointer("/color/xy/x").and_then(Value::as_f64),
            light.pointer("/color/xy/y").and_then(Value::as_f64),
        ) {
            // Converted, not passed through. The contract's `hue` is degrees and `sat` is
            // percent; the bridge answers in CIE xy — see `xy_to_hs`.
            let (hue, sat) = xy_to_hs(x, y);
            let mut a = Args::new();
            a.insert("hue".into(), json!((hue * 10.0).round() / 10.0));
            a.insert("sat".into(), json!((sat * 10.0).round() / 10.0));
            out.push(HostCall::notify(1, "color_changed", a));
        }
        out
    }

    /// Report the change immediately rather than waiting for the bridge.
    ///
    /// The bulb is on a mesh; a round trip is 100–300 ms and the UI would visibly lag. We
    /// state the intent now and let the next poll correct us if the bridge disagreed — which
    /// is what every Hue integration worth using does.
    fn optimiztic(level: u8) -> Vec<HostCall> {
        // Power as well as brightness, since core no longer infers one from the other. Without
        // this the tile waits a bridge round trip to show a lamp somebody just switched off —
        // and `level_changed` alone would now leave it reading "on" the whole time.
        let mut power = Args::new();
        power.insert("on".into(), json!(level > 0));
        let mut args = Args::new();
        args.insert("level".into(), json!(level));
        let mut out = vec![HostCall::notify(1, "power_changed", power)];
        // A level of zero is "off", not "will return at nothing" — leave the remembered
        // brightness alone so the next plain `on` restores it.
        if level > 0 {
            out.push(HostCall::notify(1, "level_changed", args));
        }
        out
    }
}

impl DriverModule for HueBulb {
    fn discover(&self, _driver_id: &str, state: &Value, input: &Args) -> (SetupStep, Value) {
        self.step(state, input)
    }

    fn setup(&self, _driver_id: &str, state: &Value, input: &Args) -> (SetupStep, Value) {
        self.step(state, input)
    }

    fn on_command(
        &self,
        inst: &mut Instance,
        _proxy: LocalId,
        cmd: &str,
        args: &Args,
    ) -> Vec<HostCall> {
        let ramp = args.get("ramp_ms").and_then(Value::as_u64);
        let last = inst
            .scratch
            .get("level")
            .and_then(Value::as_u64)
            .unwrap_or(100) as u8;

        let (body, level) = match cmd {
            "on" => {
                // Let the bridge restore its authoritative last level. The remembered value is
                // only for an immediate optimiztic notification; it is not sent as brightness.
                let restore = if last == 0 { 100 } else { last };
                (power_body(true, ramp), Some(restore))
            }
            "off" => (power_body(false, ramp), Some(0)),
            "toggle" => {
                let cur = inst
                    .scratch
                    .get("on")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let next = if cur {
                    0
                } else if last == 0 {
                    100
                } else {
                    last
                };
                (power_body(!cur, ramp), Some(next))
            }
            "set_level" => {
                let l = args.get("level").and_then(Value::as_u64).unwrap_or(0) as u8;
                (level_body(l, ramp), Some(l))
            }
            "set_cct" => {
                let k = args.get("kelvin").and_then(Value::as_u64).unwrap_or(2700);
                // The bridge speaks mireds, and clamps to the bulb's real gamut.
                let mirek = (1_000_000 / k.max(1)).clamp(153, 500);
                (json!({ "color_temperature": { "mirek": mirek } }), None)
            }
            "set_color" => {
                let h = args.get("hue").and_then(Value::as_f64).unwrap_or(0.0);
                let s = args.get("sat").and_then(Value::as_f64).unwrap_or(0.0);
                let (x, y) = hs_to_xy(h, s);
                (json!({ "color": { "xy": { "x": x, "y": y } } }), None)
            }
            "ramp_start" | "ramp_stop" => {
                // Hue has no open-ended ramp; the UI's held button becomes a long fade.
                let up = args.get("direction").and_then(Value::as_str) == Some("up");
                let target = if cmd == "ramp_stop" {
                    last
                } else if up {
                    100
                } else {
                    1
                };
                (level_body(target, Some(4000)), Some(target))
            }
            other => return vec![HostCall::warn(format!("hue: unhandled `{other}`"))],
        };

        let Some(req) = Self::request(inst, body) else {
            return vec![HostCall::warn(
                "hue: set Bridge address and Light id on this device first",
            )];
        };

        let mut out = vec![req];
        if let Some(l) = level {
            // Remember the last level the bulb was actually AT, so `on` can restore it.
            // Writing 0 here would make the next `on` come back at 1%.
            if l > 0 {
                inst.scratch.insert("level".into(), json!(l));
            }
            inst.scratch.insert("on".into(), json!(l > 0));
            out.extend(Self::optimiztic(l));
        }
        if cmd == "set_cct"
            && let Some(k) = args.get("kelvin").and_then(Value::as_u64)
        {
            let mut a = Args::new();
            a.insert("kelvin".into(), json!(k));
            out.push(HostCall::notify(1, "cct_changed", a));
        }
        if cmd == "set_color" {
            let mut a = Args::new();
            a.insert("hue".into(), args.get("hue").cloned().unwrap_or(json!(0.0)));
            a.insert("sat".into(), args.get("sat").cloned().unwrap_or(json!(0.0)));
            out.push(HostCall::notify(1, "color_changed", a));
        }
        out
    }

    fn on_group(&self, inst: &mut Instance, request: &GroupRequest) -> GroupResponse {
        if Role::of(inst) != Role::Bridge {
            return refused("Hue grouped control must run on the bridge", Value::Null);
        }
        let status = || group_status(inst, request);
        match &request.operation {
            GroupOperation::Status => GroupResponse {
                disposition: GroupDisposition::Handled,
                status: status(),
                ..Default::default()
            },
            GroupOperation::Link { resource } => {
                let desired = match group_light_ids(request) {
                    Ok(ids) => ids,
                    Err(problem) => return refused(problem, status()),
                };
                let Some(current_bridge) = bridge_id(inst).map(str::to_string) else {
                    return refused("the Hue bridge identity has not loaded yet", status());
                };
                let Some(zone) = cached_zone(inst, resource).cloned() else {
                    return refused("that Hue zone no longer exists", status());
                };
                if zone_light_ids(&zone) != desired {
                    return refused(
                        "an existing Hue zone can only be linked when its lights exactly match",
                        status(),
                    );
                }
                let Some(grouped_light) = zone_grouped_light(&zone).map(str::to_string) else {
                    return refused(
                        "that Hue zone cannot control its lights as a group",
                        status(),
                    );
                };
                set_group_link(
                    inst,
                    request.group,
                    json!({
                        "ownership": "external",
                        "bridge_id": current_bridge,
                        "zone": resource,
                        "grouped_light": grouped_light,
                        "light_ids": desired,
                        "name": zone_name(&zone),
                    }),
                );
                inst.scratch.remove(HUE_GROUP_PROBLEM);
                GroupResponse {
                    disposition: GroupDisposition::Handled,
                    status: group_status(inst, request),
                    ..Default::default()
                }
            }
            GroupOperation::Create => {
                if group_link(inst, request.group).is_some() {
                    return refused(
                        "detach the current Hue zone before creating another one",
                        status(),
                    );
                }
                if inst.scratch.contains_key(HUE_GROUP_PENDING) {
                    return refused("another Hue zone write is still in progress", status());
                }
                let light_ids = match group_light_ids(request) {
                    Ok(ids) => ids,
                    Err(problem) => return refused(problem, status()),
                };
                let Some(current_bridge) = bridge_id(inst).map(str::to_string) else {
                    return refused("the Hue bridge identity has not loaded yet", status());
                };
                let (name, token) = owned_zone_name(request.group, &request.name);
                let body = json!({
                    "metadata": { "name": name, "archetype": "other" },
                    "children": zone_children(&light_ids),
                });
                let Some(call) = bridge_http(inst, "POST", "/clip/v2/resource/zone", Some(body))
                else {
                    return refused("the Hue bridge connection is not configured", status());
                };
                inst.scratch.insert(
                    HUE_GROUP_PENDING.into(),
                    json!({
                        "operation": "create",
                        "group": request.group,
                        "bridge_id": current_bridge,
                        "light_ids": light_ids,
                        "token": token,
                        "name": name,
                    }),
                );
                GroupResponse {
                    disposition: GroupDisposition::Queued,
                    status: json!({ "pending": "create" }),
                    calls: vec![call],
                    ..Default::default()
                }
            }
            GroupOperation::Synchronize => {
                if inst.scratch.contains_key(HUE_GROUP_PENDING) {
                    return refused("another Hue zone write is still in progress", status());
                }
                let Some(link) = group_link(inst, request.group) else {
                    return refused("this group has no linked Hue zone", status());
                };
                if link.get("ownership").and_then(Value::as_str) != Some("juno") {
                    return refused(
                        "existing Hue zones stay Hue-owned and cannot be reconfigured by Juno",
                        status(),
                    );
                }
                let Some(current_bridge) = bridge_id(inst).map(str::to_string) else {
                    return refused("the Hue bridge identity has not loaded yet", status());
                };
                if link.get("bridge_id").and_then(Value::as_str) != Some(&current_bridge) {
                    return refused("the saved zone belongs to a different Hue bridge", status());
                }
                let Some(zone_id) = link.get("zone").and_then(Value::as_str).map(str::to_string)
                else {
                    return refused("the saved Hue zone link is incomplete", status());
                };
                let Some(zone) = cached_zone(inst, &zone_id) else {
                    return refused("the Juno-created Hue zone no longer exists", status());
                };
                let token = link.get("token").and_then(Value::as_str).unwrap_or("");
                if token.is_empty() || !zone_name(zone).contains(token) {
                    return refused(
                        "the Juno ownership marker was removed; refusing to modify this zone",
                        status(),
                    );
                }
                let light_ids = match group_light_ids(request) {
                    Ok(ids) => ids,
                    Err(problem) => return refused(problem, status()),
                };
                let (name, _) = owned_zone_name(request.group, &request.name);
                let body = json!({
                    "metadata": { "name": name },
                    "children": zone_children(&light_ids),
                });
                let Some(call) = bridge_http(
                    inst,
                    "PUT",
                    &format!("/clip/v2/resource/zone/{zone_id}"),
                    Some(body),
                ) else {
                    return refused("the Hue bridge connection is not configured", status());
                };
                inst.scratch.insert(
                    HUE_GROUP_PENDING.into(),
                    json!({
                        "operation": "synchronize",
                        "group": request.group,
                        "bridge_id": current_bridge,
                        "zone": zone_id,
                        "light_ids": light_ids,
                        "token": token,
                        "name": name,
                    }),
                );
                GroupResponse {
                    disposition: GroupDisposition::Queued,
                    status: json!({ "pending": "synchronize" }),
                    calls: vec![call],
                    ..Default::default()
                }
            }
            GroupOperation::Detach => {
                // Detach is deliberately local. Even a Juno-created zone remains recoverable in
                // Hue until an explicit, separately designed delete operation exists.
                remove_group_link(inst, request.group);
                inst.scratch.remove(HUE_GROUP_PROBLEM);
                GroupResponse {
                    disposition: GroupDisposition::Handled,
                    status: group_status(inst, request),
                    ..Default::default()
                }
            }
            GroupOperation::Command { command, args } => {
                let grouped_light = match validated_grouped_light(inst, request) {
                    Ok(id) => id,
                    Err(problem) => return refused(problem, status()),
                };
                let (body, members) = match grouped_command(request, command, args) {
                    Ok(plan) => plan,
                    Err(problem) => return refused(problem, status()),
                };
                let Some(call) = bridge_http(
                    inst,
                    "PUT",
                    &format!("/clip/v2/resource/grouped_light/{grouped_light}"),
                    Some(body),
                ) else {
                    return refused("the Hue bridge connection is not configured", status());
                };
                GroupResponse {
                    disposition: GroupDisposition::Handled,
                    status: status(),
                    calls: vec![call],
                    members,
                    ..Default::default()
                }
            }
        }
    }

    fn on_scene(&self, inst: &mut Instance, request: &SceneRequest) -> SceneResponse {
        scene::handle(inst, request)
    }

    /// Ask the bridge where this bulb actually is, rather than assuming.
    ///
    /// Without this a freshly adopted light shows no state until someone commands it, which
    /// reads as broken — and it is: the bulb may well already be on.
    fn on_bind(&self, inst: &mut Instance) -> Vec<HostCall> {
        let role = Role::of(inst);

        // Every binding, not just the first. A motion sensor is three of them and a Tap Dial five,
        // and a binding that never said it was reachable is drawn as a device that is not there.
        let mut out: Vec<HostCall> = role
            .bindings(inst)
            .into_iter()
            .map(|binding| {
                let mut a = Args::new();
                a.insert("online".into(), json!(true));
                HostCall::notify(binding, "online_changed", a)
            })
            .collect();

        // The bridge — the one instance with no resource id of its own — opens the event stream,
        // once, for the whole house.
        //
        // Nothing here is polled, and until this existed nothing was: a bulb changed in the
        // Hue app or at a wall switch never got back to Juno, because the only thing that ever
        // reported a level was this driver stating its own intent after a command. One
        // subscription is what Hue offers and all it wants — core hands every frame to the
        // bulbs behind this bridge, and each keeps the ones naming it.
        if role == Role::Bridge
            && let (Some(bridge), Some(key)) = (
                inst.property("Bridge address").as_str(),
                inst.property("Application key").as_str(),
            )
        {
            let request = format!(
                "GET /eventstream/clip/v2 HTTP/1.1\r\n\
                 Host: {bridge}\r\n\
                 Accept: text/event-stream\r\n\
                 Cache-Control: no-cache\r\n\
                 hue-application-key: {key}\r\n\r\n"
            );
            out.push(HostCall::Tx {
                control: 0,
                data: request.into_bytes(),
            });

            // The v1 config endpoint is the bridge's durable identity. A zone UUID plus a local
            // ownership record is not enough if somebody points this device at a replacement
            // bridge later; binding the record to `bridgeid` closes that accidental write path.
            out.push(HostCall::Http(HttpRequest::new("GET", "/api/config")));

            // And one read of each kind of state, so a freshly started controller knows where the
            // house stands without waiting for something to change.
            //
            // One request per collection, not one per device. This used to be the bulb's own job,
            // which meant twenty-four simultaneous GETs at every start — the bridge answered some
            // of them with 429 and the rest arrived as a burst it had no reason to be asked for.
            // The collection endpoints return everything in one answer each, and core hands a
            // bridge's answer to the devices behind it, so each one still picks itself out.
            for collection in AT_START {
                out.push(HostCall::Http(
                    HttpRequest::new("GET", format!("/clip/v2/resource/{collection}"))
                        .header("hue-application-key", key),
                ));
            }
            return out;
        }

        // Everything behind the bridge asks for nothing. Its state arrives with the reads above,
        // and every change after that arrives on the stream.
        out
    }

    /// The bridge answering a state read. Also how a light someone changed in the Hue app
    /// gets back to us on the next poll.
    fn on_event(
        &self,
        inst: &mut Instance,
        _control: LocalId,
        note: &str,
        args: &Args,
    ) -> Vec<HostCall> {
        // A frame off the bridge's event stream. Core hands it to every device behind that
        // bridge, so the first thing to do is find out whether any of it is about this bulb.
        if note == "rx" {
            return self.on_stream(inst, args);
        }
        if note != "http_response" {
            return Vec::new();
        }
        if Role::of(inst) == Role::Bridge {
            let body = args.get("body").cloned().unwrap_or(Value::Null);
            if let Some(id) = body.get("bridgeid").and_then(Value::as_str) {
                inst.scratch.insert(HUE_BRIDGE_ID.into(), json!(id));
            }

            let url = args.get("url").and_then(Value::as_str).unwrap_or("");
            let method = args.get("method").and_then(Value::as_str).unwrap_or("");
            let status = args.get("status").and_then(Value::as_u64).unwrap_or(200);
            if url.contains("/clip/v2/resource/zone") {
                if scene::zone_write_pending(inst) {
                    return scene::on_zone_response(inst, args);
                }
                if status >= 400 {
                    inst.scratch.insert(
                        HUE_GROUP_PROBLEM.into(),
                        json!(format!("Hue rejected the zone write with HTTP {status}")),
                    );
                    inst.scratch.remove(HUE_GROUP_PENDING);
                    return Vec::new();
                }
                if method.eq_ignore_ascii_case("POST") {
                    let created = body
                        .get("data")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .find(|resource| {
                            resource.get("rtype").and_then(Value::as_str) == Some("zone")
                                || resource.get("type").and_then(Value::as_str) == Some("zone")
                        })
                        .and_then(|resource| {
                            resource
                                .get("rid")
                                .or_else(|| resource.get("id"))
                                .and_then(Value::as_str)
                        })
                        .map(str::to_string);
                    if let (Some(zone), Some(pending)) = (
                        created,
                        inst.scratch
                            .get_mut(HUE_GROUP_PENDING)
                            .and_then(Value::as_object_mut),
                    ) {
                        pending.insert("zone".into(), json!(zone));
                    } else {
                        inst.scratch.insert(
                            HUE_GROUP_PROBLEM.into(),
                            json!("Hue did not identify the zone it created"),
                        );
                        inst.scratch.remove(HUE_GROUP_PENDING);
                        return Vec::new();
                    }
                    return bridge_http(inst, "GET", "/clip/v2/resource/zone", None)
                        .into_iter()
                        .collect();
                }
                if method.eq_ignore_ascii_case("PUT") {
                    return bridge_http(inst, "GET", "/clip/v2/resource/zone", None)
                        .into_iter()
                        .collect();
                }
                if method.eq_ignore_ascii_case("GET")
                    && let Some(data) = body.get("data").and_then(Value::as_array)
                {
                    cache_zone_inventory(inst, data);
                    return Vec::new();
                }
            }
            if let Some(calls) = scene::on_collection_response(inst, args) {
                return calls;
            }
        }
        // CLIP v2 answers `{"errors": [], "data": [ … ]}` — one entry for a single resource,
        // everything of that type for a collection. Core hands a bridge's answer to the devices
        // behind it, so this is reached with the whole house in it and has to find its own lines.
        //
        // Plural, because a motion sensor has three: one answer to the temperature read carries
        // every sensor in the house, and the same pass serves all five collections the bridge asks
        // for at start. Anything not naming this device produces nothing.
        let Some(data) = args
            .get("body")
            .and_then(|b| b.get("data"))
            .and_then(Value::as_array)
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for resource in data {
            // Always `update`, never `add`. A poll is a snapshot of what is already there, and
            // the collection reads at start return the whole house — treating those as additions
            // would offer every bulb already adopted, as new, on every controller restart.
            out.extend(Self::mine(inst, "update", resource));
        }
        out
    }
}

export_driver!(HueBulb);


#[cfg(test)]
mod power_tests {
    use super::*;

    fn inst() -> Instance {
        let mut inst = Instance::default();
        inst.properties.insert("Bridge address".into(), json!("10.0.0.2"));
        inst.properties.insert("Application key".into(), json!("k"));
        inst.properties.insert("Light id".into(), json!("l1"));
        inst
    }

    fn notes(calls: &[HostCall]) -> Vec<(String, Args)> {
        calls
            .iter()
            .filter_map(|c| match c {
                HostCall::Notify { name, args, .. } => Some((name.clone(), args.clone())),
                _ => None,
            })
            .collect()
    }

    /// A bulb switched off in the Hue app still reports the brightness it will return to.
    /// Core used to infer power from that level and turn the lamp back on on screen.
    #[test]
    fn an_off_bulb_reports_off_and_keeps_its_brightness() {
        let mut inst = inst();
        let light = json!({ "on": { "on": false }, "dimming": { "brightness": 47.0 } });
        let said = notes(&HueBulb::report(&mut inst, &light));

        let power = said.iter().find(|(n, _)| n == "power_changed").expect("says it is off");
        assert_eq!(power.1.get("on"), Some(&json!(false)));

        let level = said.iter().find(|(n, _)| n == "level_changed").expect("and how bright");
        assert_eq!(
            level.1.get("level"),
            Some(&json!(47)),
            "the level it will come back at, not zero standing in for off",
        );
    }

    #[test]
    fn an_on_bulb_reports_on() {
        let mut inst = inst();
        let light = json!({ "on": { "on": true }, "dimming": { "brightness": 80.0 } });
        let said = notes(&HueBulb::report(&mut inst, &light));
        assert_eq!(
            said.iter().find(|(n, _)| n == "power_changed").unwrap().1.get("on"),
            Some(&json!(true)),
        );
    }
}

#[cfg(test)]
mod pairing_tests {
    use super::*;

    fn offered(resource: Value) -> Option<(String, String, String)> {
        match HueBulb::offer(&resource).into_iter().next()? {
            HostCall::Notify { name, args, .. } => {
                assert_eq!(name, "device_added");
                let s = |k: &str| {
                    args.get(k)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string()
                };
                Some((s("id"), s("name"), s("kind")))
            }
            _ => None,
        }
    }

    #[test]
    fn a_paired_bulb_is_offered_once_and_not_six_times() {
        // What the bridge actually pushes when one bulb is paired in the Hue app: the device
        // and every service hanging off it, each as its own `add`. Only the device is a thing
        // a person would recognize as "a new light".
        let device = json!({
            "id": "b8b7-1", "type": "device",
            "metadata": { "name": "Porch" },
            "product_data": { "product_name": "Hue color lamp", "product_archetype": "sultan_bulb" }
        });
        assert_eq!(
            offered(device),
            Some(("b8b7-1".into(), "Porch".into(), "sultan_bulb".into()))
        );
        for service in [
            "light",
            "zigbee_connectivity",
            "entertainment",
            "taurus_7455",
        ] {
            assert_eq!(
                HueBulb::offer(&json!({ "id": "x", "type": service })).len(),
                0
            );
        }
    }

    #[test]
    fn a_bulb_left_at_its_default_name_is_still_named() {
        // No `metadata.name` because nobody typed one. Falling through to the UUID would put
        // "0b216d8e-… was added" on somebody's phone.
        let (_, name, _) = offered(json!({
            "id": "0b216d8e", "type": "device",
            "product_data": { "product_name": "Hue dimmer switch" }
        }))
        .expect("a device with no chosen name is still offered");
        assert_eq!(name, "Hue dimmer switch");
    }

    /// An instance with the ids a device of this role would have been adopted with.
    fn adopted(pairs: &[(&str, &str)]) -> Instance {
        let mut inst = Instance::default();
        for (property, value) in pairs {
            inst.properties
                .insert((*property).to_string(), json!(value));
        }
        inst
    }

    fn deleted(inst: &mut Instance, rid: &str, kind: &str) -> bool {
        let frame = json!({ "id": rid, "type": kind });
        matches!(
            HueBulb::mine(inst, "delete", &frame).first(),
            Some(HostCall::Gone { .. })
        )
    }

    #[test]
    fn a_bulb_unpaired_at_the_bridge_removes_itself_and_only_itself() {
        let mut mine = adopted(&[("Light id", "L1")]);
        assert!(deleted(&mut mine, "L1", "light"));
        // The frame reaches every device behind the bridge. One bulb being unpaired must not
        // take the rest of the house with it — the same rule that makes one stream serve
        // twenty-four devices.
        assert!(!deleted(&mut mine, "L2", "light"));
    }

    #[test]
    fn any_one_service_is_enough_to_remove_a_multi_sensor() {
        // One piece of hardware, three services, and unpairing deletes all three. Whichever
        // arrives first is the one that removes it; waiting for all three would leave the
        // device half-gone if the bridge only named two.
        for rid in ["M1", "T1", "LL1"] {
            let mut sensor = adopted(&[
                ("Motion id", "M1"),
                ("Temperature id", "T1"),
                ("Light level id", "LL1"),
            ]);
            assert!(
                deleted(&mut sensor, rid, "motion"),
                "{rid} should remove it"
            );
        }
    }

    #[test]
    fn the_bridge_does_not_remove_what_it_merely_hears_about() {
        // It hears every deletion in the house on its own connection. Acting on them would let
        // one unpaired bulb remove the hub — and everything behind it with it.
        let mut bridge = adopted(&[("Bridge address", "10.0.0.2")]);
        assert!(!deleted(&mut bridge, "L1", "light"));
        assert!(!deleted(&mut bridge, "10.0.0.2", "device"));
    }

    #[test]
    fn a_poll_never_offers_anything() {
        // The guard that keeps a controller restart quiet: the five collection reads at start
        // return every adopted device, and they arrive as `update`.
        let mut inst = Instance::default(); // no ids set, so Role::of is Bridge
        let device = json!({ "id": "b8b7-1", "type": "device", "metadata": { "name": "Porch" } });
        assert_eq!(HueBulb::mine(&mut inst, "update", &device).len(), 0);
        assert_eq!(HueBulb::mine(&mut inst, "add", &device).len(), 1);
    }
}

// ---------------------------------------------------------------------------------------
// Setup flow
// ---------------------------------------------------------------------------------------

/// Where the flow is. Core carries this between calls; the driver stays stateless.
fn phase(state: &Value) -> &str {
    state
        .get("phase")
        .and_then(Value::as_str)
        .unwrap_or("start")
}

/// Finish, asking first whether the bridge's scenes should come too.
///
/// Asked rather than assumed, because Signify creates four or five in every room whether anybody
/// wanted them or not, and a house that gained sixty scenes by adopting a bridge would be a mess
/// somebody has to clean up by hand. The ones that survive this far already touch a light that was
/// actually picked, so the question is only ever about scenes that would work.
///
/// Skipped entirely when there are none, which is most bridges browsed for a single bulb — a
/// screen that only ever has one answer is a screen not worth showing.
fn ask_about_scenes(
    devices: Vec<Candidate>,
    rules: Vec<ImportedRule>,
    scenes: Vec<ImportedScene>,
) -> (SetupStep, Value) {
    if scenes.is_empty() {
        return (
            SetupStep::Done {
                devices,
                rules,
                scenes,
            },
            Value::Null,
        );
    }

    let names: Vec<&str> = scenes.iter().take(4).map(|s| s.title.as_str()).collect();
    let n = scenes.len();
    (
        SetupStep::Form {
            title: format!("Bring over {n} scene{}?", if n == 1 { "" } else { "s" }),
            body: format!(
                "The bridge has {n} saved — {}{}. They stay owned by Hue: Juno can recall them \
                 statically or dynamically, but can never edit or delete them. Skipping this \
                 changes nothing else; the lights and remotes are added either way.",
                names.join(", "),
                if n > names.len() { " and others" } else { "" }
            ),
            fields: vec![Field {
                name: "scenes".into(),
                label: "Scenes".into(),
                kind: "choice".into(),
                help: String::new(),
                default: Some(json!("Bring them over")),
                options: vec!["Bring them over".into(), "Leave them".into()],
                required: true,
            }],
        },
        json!({
            "phase": "scene_choice",
            "devices": devices,
            "rules": rules,
            "scenes": scenes,
        }),
    )
}

/// "accessory" / "accessories", for a count that is only known at runtime.
fn plural(n: usize) -> &'static str {
    if n == 1 { "y" } else { "ies" }
}

fn field(name: &str, label: &str, help: &str) -> Field {
    Field {
        name: name.into(),
        label: label.into(),
        kind: "string".into(),
        help: help.into(),
        default: None,
        options: Vec::new(),
        required: true,
    }
}

impl HueBulb {
    /// Offer whatever announced itself, and let an address be typed anyway.
    ///
    /// Core scans for the `_hue._tcp` service this driver's manifest declares and hands the
    /// results in. Nobody should have to go and find an IP for hardware that is already
    /// shouting its own name on the network — but multicast is blocked on plenty of networks,
    /// so typing one has to keep working.
    fn ask_for_address(state: &Value) -> (SetupStep, Value) {
        let found: Vec<&Value> = state
            .get("mdns_candidates")
            .and_then(Value::as_array)
            .map(|v| v.iter().collect())
            .unwrap_or_default();

        let typed = Field {
            name: "address".into(),
            label: "Bridge address".into(),
            kind: "string".into(),
            help: "for example 192.168.1.42".into(),
            default: None,
            options: Vec::new(),
            required: true,
        };

        if found.is_empty() {
            return (
                SetupStep::Form {
                    title: "Find your Hue bridge".into(),
                    body: "Nothing announced itself on the network, so enter the bridge's \
                           address. It is in the Hue app under Settings → My Hue System → the \
                           (i) beside your bridge."
                        .into(),
                    fields: vec![typed],
                },
                json!({ "phase": "probe" }),
            );
        }

        // A table rather than a list: two bridges are told apart by their address and model,
        // and a single line of text cannot show both.
        let rows: Vec<PickRow> = found
            .iter()
            .filter_map(|f| {
                let address = f.get("address")?.as_str()?.to_string();
                let name = f
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("Hue bridge");
                // The advertised name is the full service instance; lead with the readable part.
                let short = name.split('.').next().unwrap_or(name).to_string();
                let txt = f.get("txt");
                let model = txt
                    .and_then(|t| t.get("modelid"))
                    .and_then(Value::as_str)
                    .unwrap_or("—")
                    .to_string();
                let id = txt
                    .and_then(|t| t.get("bridgeid"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                Some(PickRow {
                    value: address.clone(),
                    cells: vec![short, address.clone(), model],
                    // A loopback address is this machine, not a bridge anyone else can reach.
                    note: if address.starts_with("127.") {
                        "on this machine only".into()
                    } else if id.is_empty() {
                        String::new()
                    } else {
                        format!("id {id}")
                    },
                })
            })
            .collect();

        (
            SetupStep::Pick {
                title: format!(
                    "Found {} Hue bridge{}",
                    rows.len(),
                    if rows.len() == 1 { "" } else { "s" }
                ),
                body: "Pick the one to set up.".into(),
                columns: vec!["Bridge".into(), "Address".into(), "Model".into()],
                rows,
                field: "address".into(),
                manual: Some(typed),
            },
            json!({ "phase": "probe" }),
        )
    }
}

impl HueBulb {
    /// One step of the flow. Everything Hue-specific lives here rather than in the controller.
    fn step(&self, state: &Value, input: &Args) -> (SetupStep, Value) {
        let address = state
            .get("address")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                input
                    .get("address")
                    .and_then(Value::as_str)
                    .map(|s| s.trim().to_string())
            });

        // Browsing a bridge that already exists: it has an address and a key, so go straight
        // to listing. Making someone press the link button again to add a second bulb would
        // be pointless — the pairing has not expired.
        if phase(state) == "start" && state.get("browse").and_then(Value::as_bool) == Some(true) {
            let addr = state
                .get("Bridge address")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let key = state
                .get("Application key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if addr.is_empty() || key.is_empty() {
                return (
                    SetupStep::Failed {
                        reason: "this bridge has no address or key set — pair it first".into(),
                    },
                    Value::Null,
                );
            }
            return (
                SetupStep::Fetch {
                    request: HttpRequest::new(
                        "GET",
                        format!("https://{addr}/clip/v2/resource/device"),
                    )
                    .header("hue-application-key", &key),
                    note: "reading what is paired to the bridge".into(),
                },
                json!({ "phase": "devices", "address": addr, "key": key,
                        "browse": true, "parent": state.get("parent") }),
            );
        }

        match phase(state) {
            // Nothing known yet: ask where the bridge is.
            "start" => Self::ask_for_address(state),

            // Confirm it is a bridge before asking anyone to press anything.
            "probe" => {
                let Some(address) = address else {
                    return Self::ask_for_address(state);
                };
                (
                    SetupStep::Fetch {
                        request: HttpRequest::new("GET", format!("https://{address}/api/config")),
                        note: "checking the bridge".into(),
                    },
                    json!({ "phase": "probed", "address": address }),
                )
            }

            "probed" => {
                let address = address.unwrap_or_default();
                let model = input
                    .get("response")
                    .and_then(|r| r.get("modelid"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                // Every Hue bridge reports a BSB model id. Anything else is not one.
                if !model.starts_with("BSB") {
                    return (
                        SetupStep::Failed {
                            reason: format!(
                                "{address} did not answer as a Hue bridge. Check the address — \
                                 the Hue app shows it under Settings → My Hue System."
                            ),
                        },
                        Value::Null,
                    );
                }
                let name = input
                    .get("response")
                    .and_then(|r| r.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("Hue Bridge")
                    .to_string();
                (
                    SetupStep::Instruct {
                        title: format!("Press the link button on {name}"),
                        // Short on purpose. Somebody is standing at the bridge with a finger
                        // out; the reason it works this way is not what they need right now.
                        body: "The round button on top.".into(),
                        continue_label: "I pressed it".into(),
                    },
                    json!({ "phase": "pair", "address": address, "name": name }),
                )
            }

            // Ask for a key. The bridge refuses until the button is pressed.
            "pair" => {
                let address = address.unwrap_or_default();
                (
                    SetupStep::Fetch {
                        request: HttpRequest::new("POST", format!("https://{address}/api")).json(
                            json!({ "devicetype": "juno#controller",
                                    "generateclientkey": true })
                            .to_string(),
                        ),
                        note: "pairing".into(),
                    },
                    json!({ "phase": "paired", "address": address,
                            "attempt": state.get("attempt").and_then(Value::as_u64).unwrap_or(0) + 1 }),
                )
            }

            "paired" => {
                let address = address.unwrap_or_default();
                let attempt = state.get("attempt").and_then(Value::as_u64).unwrap_or(1);
                let first = input.get("response").and_then(|r| r.get(0)).cloned();

                if let Some(key) = first
                    .as_ref()
                    .and_then(|f| f.pointer("/success/username"))
                    .and_then(Value::as_str)
                {
                    return (
                        SetupStep::Fetch {
                            request: HttpRequest::new(
                                "GET",
                                format!("https://{address}/clip/v2/resource/device"),
                            )
                            .header("hue-application-key", key),
                            note: "reading what is paired to the bridge".into(),
                        },
                        json!({ "phase": "devices", "address": address, "key": key }),
                    );
                }

                let description = first
                    .as_ref()
                    .and_then(|f| f.pointer("/error/description"))
                    .and_then(Value::as_str)
                    .unwrap_or("the bridge did not answer");

                if description.contains("link button") {
                    // Keep asking for about half a minute — long enough to walk to the bridge.
                    if attempt < 30 {
                        return (
                            SetupStep::Wait {
                                title: "Waiting for the link button".into(),
                                body: "Press the round button on top of the bridge.".into(),
                                retry_ms: 1000,
                            },
                            json!({ "phase": "pair", "address": address, "attempt": attempt }),
                        );
                    }
                    return (
                        SetupStep::Failed {
                            reason: "the link button was not pressed in time — start again".into(),
                        },
                        Value::Null,
                    );
                }
                (
                    SetupStep::Failed {
                        reason: description.to_string(),
                    },
                    Value::Null,
                )
            }

            // Everything paired to the bridge that is not a bulb — sensors, dimmers, wall modules,
            // dials. One request rather than one per resource type, because a device entry names
            // its own services and so arrives already grouped by the thing it is part of.
            "devices" => {
                let address = address.unwrap_or_default();
                let key = state
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let found = catalog::compact(input.get("response"));
                (
                    SetupStep::Fetch {
                        request: HttpRequest::new(
                            "GET",
                            format!("https://{address}/clip/v2/resource/button"),
                        )
                        .header("hue-application-key", &key),
                        note: "reading the buttons".into(),
                    },
                    json!({
                        "phase": "buttons",
                        "address": address,
                        "key": key,
                        "catalog": found,
                        "browse": state.get("browse").and_then(Value::as_bool).unwrap_or(false),
                        "parent": state.get("parent"),
                    }),
                )
            }

            // Which button is which. The device entry lists a remote's buttons as an unordered set,
            // and only the `/button` collection carries `metadata.control_id` — so without this
            // step every rule in the house would be attached to an arbitrary button and the remote
            // would look faulty.
            "buttons" => {
                let address = address.unwrap_or_default();
                let key = state
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut found: Vec<Value> = state
                    .get("catalog")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                catalog::order_buttons(&mut found, input.get("response"));
                (
                    SetupStep::Fetch {
                        request: HttpRequest::new(
                            "GET",
                            format!("https://{address}/clip/v2/resource/room"),
                        )
                        .header("hue-application-key", &key),
                        note: "reading the rooms".into(),
                    },
                    json!({
                        "phase": "rooms",
                        "address": address,
                        "key": key,
                        "catalog": found,
                        "browse": state.get("browse").and_then(Value::as_bool).unwrap_or(false),
                        "parent": state.get("parent"),
                    }),
                )
            }

            // Where the bridge says everything lives.
            //
            // The step that decides whether adopting a house is one press or an afternoon. A Hue
            // bridge is usually the only one in the building and its bulbs are named by the app —
            // "Hue color lamp 3", forty times — so the room is the only thing distinguishing one
            // row from another, and somebody already filed all of it once in the Hue app.
            "rooms" => {
                let address = address.unwrap_or_default();
                let key = state
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut found: Vec<Value> = state
                    .get("catalog")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                catalog::assign_rooms(&mut found, input.get("response"));
                let names = catalog::room_names(input.get("response"));
                (
                    SetupStep::Fetch {
                        request: HttpRequest::new(
                            "GET",
                            format!("https://{address}/clip/v2/resource/zone"),
                        )
                        .header("hue-application-key", &key),
                        note: "reading the zones".into(),
                    },
                    json!({
                        "phase": "zones",
                        "address": address,
                        "key": key,
                        "catalog": found,
                        "rooms": names,
                        "browse": state.get("browse").and_then(Value::as_bool).unwrap_or(false),
                        "parent": state.get("parent"),
                    }),
                )
            }

            // The other way somebody groups lights in the Hue app.
            //
            // A room holds *devices* and is where a bulb physically is; a zone holds light
            // *services* and is any grouping somebody wanted — "Downstairs", "Lamps". Plenty of
            // houses use only zones, and reading rooms alone brought every one of those in
            // unplaced, throwing away filing somebody had already done once.
            //
            // Rooms are applied first and are not overwritten: where a bulb is beats what it has
            // been grouped with.
            "zones" => {
                let address = address.unwrap_or_default();
                let key = state
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut found: Vec<Value> = state
                    .get("catalog")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                catalog::assign_rooms(&mut found, input.get("response"));
                // A zone's name is worth offering as a room to file things under, the same as a
                // room's — it is what somebody called that group.
                let names = catalog::merge_names(
                    state.get("rooms"),
                    catalog::room_names(input.get("response")),
                );
                (
                    SetupStep::Fetch {
                        request: HttpRequest::new(
                            "GET",
                            format!("https://{address}/clip/v2/resource/behavior_instance"),
                        )
                        .header("hue-application-key", &key),
                        note: "reading what the switches already control".into(),
                    },
                    json!({
                        "phase": "behaviors",
                        "address": address,
                        "key": key,
                        "catalog": found,
                        "rooms": names,
                        "browse": state.get("browse").and_then(Value::as_bool).unwrap_or(false),
                        "parent": state.get("parent"),
                    }),
                )
            }

            // What the bridge's own automations wire each switch to.
            //
            // Not imported as rules — Hue's semantics are not Juno's, and a rule whose origin
            // nobody can see is worse than no rule. It is read for what it says about identity and
            // place: "controls the Kitchen" is a usable name for a thing the app called "Hue
            // dimmer switch 2", and for a battery remote sitting in no Hue room at all it is the
            // best answer available to where the thing is.
            "behaviors" => {
                let address = address.unwrap_or_default();
                let key = state
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut found: Vec<Value> = state
                    .get("catalog")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let names = state.get("rooms").cloned().unwrap_or(Value::Null);
                catalog::apply_behaviors(&mut found, input.get("response"), &names);
                (
                    SetupStep::Fetch {
                        request: HttpRequest::new(
                            "GET",
                            format!("https://{address}/clip/v2/resource/scene"),
                        )
                        .header("hue-application-key", &key),
                        note: "reading the scenes".into(),
                    },
                    json!({
                        "phase": "hue_scenes",
                        "address": address,
                        "key": key,
                        "catalog": found,
                        "rooms": names,
                        "browse": state.get("browse").and_then(Value::as_bool).unwrap_or(false),
                        "parent": state.get("parent"),
                    }),
                )
            }

            // The bridge's own scenes, kept raw until it is known which bulbs were picked.
            //
            // A scene is a list of light services and what each should be doing, and a light
            // service only becomes something a scene can name once somebody has adopted it — so
            // this cannot be reduced yet, only carried.
            "hue_scenes" => {
                let address = address.unwrap_or_default();
                let key = state
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                (
                    SetupStep::Fetch {
                        request: HttpRequest::new(
                            "GET",
                            format!("https://{address}/clip/v2/resource/light"),
                        )
                        .header("hue-application-key", &key),
                        note: "reading the light list".into(),
                    },
                    json!({
                        "phase": "lights",
                        "address": address,
                        "key": key,
                        "catalog": state.get("catalog"),
                        "rooms": state.get("rooms"),
                        "hue_scenes": input.get("response"),
                        "browse": state.get("browse").and_then(Value::as_bool).unwrap_or(false),
                        "parent": state.get("parent"),
                    }),
                )
            }

            // The real bulbs, read off the bridge. CLIP v2 answers
            // `{"errors": [], "data": [ … ]}`, each entry carrying a UUID and its own state.
            // The accessories gathered by the two steps above are offered alongside them, so
            // somebody setting a bridge up picks everything once instead of going round three times.
            "lights" => {
                let address = address.unwrap_or_default();
                let key = state
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let response = input.get("response");

                if let Some(err) = response
                    .and_then(|r| r.get("errors"))
                    .and_then(Value::as_array)
                    .and_then(|e| e.first())
                    .and_then(|e| e.get("description"))
                    .and_then(Value::as_str)
                {
                    return (
                        SetupStep::Failed {
                            reason: format!("the bridge refused the light list: {err}"),
                        },
                        Value::Null,
                    );
                }

                // No lights is no longer a failure. A bridge with a motion sensor and a dimmer on it
                // and no bulbs of its own is a real setup — somebody using Hue accessories to drive
                // Lutron loads — and refusing it because one of the three collections came back
                // empty would be refusing a house that works.
                let data: Vec<Value> = response
                    .and_then(|r| r.get("data"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();

                // Read before the bulbs are built, because each of them wants its room out of it.
                // A bulb is offered by its *light service*, and a Hue room lists *devices* — so the
                // hop from one to the other goes through the device that owns the service, which is
                // exactly what the catalog recorded two steps ago.
                let found: Vec<Value> = state
                    .get("catalog")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();

                let mut options: Vec<Candidate> = data
                    .iter()
                    .filter_map(|light| {
                        let id = light.get("id")?.as_str()?;
                        let name = light
                            .pointer("/metadata/name")
                            .and_then(Value::as_str)
                            .unwrap_or("Hue light");
                        let on = light.pointer("/on/on").and_then(Value::as_bool);
                        // v2 omits `on` for a light the bridge cannot currently see.
                        // Only worth a word when something is wrong. "The bridge reports it
                        // on" against every row is a column of identical text beside the one
                        // thing that tells them apart, which is the name.
                        // Now that they are all one driver, what kind of light this is has to
                        // be said somewhere — a list of forty rows called "Hue color lamp 1"
                        // used to carry it in the driver name.
                        let state = match on {
                            None => "cannot be reached — is it powered?".to_string(),
                            _ => bulb_shape(light).to_string(),
                        };
                        // A Hue `light` is not necessarily a color bulb. The manifest is the
                        // source of the controls core accepts, so this choice must happen here
                        // at import time — hiding a color wheel in the UI would still let a
                        // rule send `set_color` to a white bulb. `null` is not a capability;
                        // the bridge uses it for a resource it cannot describe at the moment.
                        Some(Candidate {
                            label: name.to_string(),
                            kind: "light".into(),
                            driver_id: "signify.hue.light".into(),
                            capabilities: bulb_capabilities(light),
                            properties: [
                                ("Bridge address".to_string(), json!(address)),
                                ("Application key".to_string(), json!(key)),
                                ("Light id".to_string(), json!(id)),
                            ]
                            .into_iter()
                            .collect(),
                            verified: state,
                            room: catalog::room_of_light(&found, id).unwrap_or_default(),
                        })
                    })
                    .collect();
                options.sort_by(|a, b| a.label.cmp(&b.label));
                let bulbs = options.len();

                // The sensors and controls gathered earlier, already sorted among themselves.
                let accessories = catalog::candidates(&found, &address, &key);
                let extras = accessories.len();
                options.extend(accessories);

                if options.is_empty() {
                    return (
                        SetupStep::Failed {
                            reason: "the bridge reported nothing paired to it — add your lights \
                                     and accessories in the Hue app first"
                                .into(),
                        },
                        Value::Null,
                    );
                }

                let title = match (bulbs, extras) {
                    (b, 0) => format!("{b} light(s) on this bridge"),
                    (0, e) => format!("{e} accessor{} on this bridge", plural(e)),
                    (b, e) => format!("{b} light(s) and {e} accessor{} on this bridge", plural(e)),
                };

                (
                    SetupStep::Choose {
                        title,
                        body: "Anything the bridge cannot reach is marked.".into(),
                        options,
                        multiple: true,
                    },
                    json!({
                        "phase": "chosen",
                        "address": address,
                        "key": key,
                        "catalog": state.get("catalog"),
                        "rooms": state.get("rooms"),
                        "hue_scenes": state.get("hue_scenes"),
                        // Carried through, or the last step forgets it is browsing and
                        // offers a second copy of a bridge that is already set up.
                        "browse": state.get("browse").and_then(Value::as_bool).unwrap_or(false),
                        "parent": state.get("parent"),
                    }),
                )
            }

            "chosen" => {
                let address = address.unwrap_or_default();
                let key = state
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let chosen: Vec<Candidate> = input
                    .get("chosen")
                    .and_then(|c| driver_sdk::serde_json::from_value(c.clone()).ok())
                    .unwrap_or_default();

                // The bridge comes first and carries the connection; everything behind it carries
                // only what makes it individual. Core adopts the parent, then attaches the rest.
                // Browsing an existing bridge: only the children are new.
                //
                // Each candidate keeps the `driver_id` the step that built it chose. It used to be
                // overwritten with the bulb driver here, which was harmless while bulbs were the
                // only thing on offer and would now quietly turn every sensor and every keypad into
                // a light that 404s on its first command.
                // The rules the bridge already has, for whatever was actually picked. Built before
                // the inherited properties are stripped, because that is what identifies a device.
                let catalog: Vec<Value> = state
                    .get("catalog")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();

                if state.get("browse").and_then(Value::as_bool) == Some(true) {
                    let rules = catalog::rules(&catalog, &chosen);
                    let scenes = catalog::scenes(state.get("hue_scenes"), &chosen);
                    let devices: Vec<Candidate> = chosen
                        .into_iter()
                        .map(|mut c| {
                            c.properties.remove("Bridge address");
                            c.properties.remove("Application key");
                            c
                        })
                        .collect();
                    return ask_about_scenes(devices, rules, scenes);
                }

                let mut devices = vec![Candidate {
                    label: state
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("Hue Bridge")
                        .to_string(),
                    kind: "bridge".into(),
                    driver_id: "signify.hue.bridge".into(),
                    properties: [
                        ("Bridge address".to_string(), json!(address)),
                        ("Application key".to_string(), json!(key)),
                    ]
                    .into_iter()
                    .collect(),
                    capabilities: Default::default(),
                    verified: format!("{} device(s) behind it", chosen.len()),
                    // A bridge lives in a cupboard and serves the whole house. Core refuses to
                    // place infrastructure anyway; saying nothing here is the same answer said
                    // once rather than twice.
                    room: String::new(),
                }];

                // Indices are into the list core is handed, and the bridge is the first entry of
                // it — so the rules are built against that list rather than against `chosen`,
                // which is one shorter and would point every rule at the wrong device.
                for mut c in chosen {
                    // Drop the inherited copies — the bridge holds them now.
                    c.properties.remove("Bridge address");
                    c.properties.remove("Application key");
                    devices.push(c);
                }
                let rules = catalog::rules(&catalog, &devices);
                let scenes = catalog::scenes(state.get("hue_scenes"), &devices);
                ask_about_scenes(devices, rules, scenes)
            }

            // The answer to that question.
            "scene_choice" => {
                let take = input
                    .get("scenes")
                    .and_then(Value::as_str)
                    .is_some_and(|a| a.starts_with("Bring"));
                let devices: Vec<Candidate> = state
                    .get("devices")
                    .cloned()
                    .and_then(|v| driver_sdk::serde_json::from_value(v).ok())
                    .unwrap_or_default();
                let rules: Vec<ImportedRule> = state
                    .get("rules")
                    .cloned()
                    .and_then(|v| driver_sdk::serde_json::from_value(v).ok())
                    .unwrap_or_default();
                let scenes: Vec<ImportedScene> = match take {
                    false => Vec::new(),
                    true => state
                        .get("scenes")
                        .cloned()
                        .and_then(|v| driver_sdk::serde_json::from_value(v).ok())
                        .unwrap_or_default(),
                };
                (
                    SetupStep::Done {
                        devices,
                        rules,
                        scenes,
                    },
                    Value::Null,
                )
            }

            other => (
                SetupStep::Failed {
                    reason: format!("unknown setup phase `{other}`"),
                },
                Value::Null,
            ),
        }
    }
}

/// What one CLIP v2 light can actually do.
///
/// Philips sells a light. It sells a few different ones — some take color, some only warm up
/// and cool down, some are a switched socket with a bulb in it — but a person shopping for
/// them, or looking at a list of drivers to install, is looking for a light. There were five
/// manifests here, one per shape, differing in a single capability line.
///
/// So the shapes are answered per light instead, which is where the answer was all along: the
/// bridge says which resources a fitting has. Absent and `null` both mean no — the bridge uses
/// `null` for a feature it cannot describe just now, and a color gamut that might be there is
/// not a color bulb.
///
/// This has to be decided here rather than hidden in a control: the resolved contract is what
/// core validates against, so a light whose driver claims `set_color` can be sent one by a
/// rule whatever any screen chooses to draw.
fn bulb_capabilities(light: &Value) -> BTreeMap<String, Value> {
    let has = |name: &str| light.get(name).and_then(Value::as_object).is_some();
    let (dimmer, color, cct) = (has("dimming"), has("color"), has("color_temperature"));

    let mut caps: BTreeMap<String, Value> = [
        ("dimmer".to_string(), json!(dimmer)),
        ("supports_color".to_string(), json!(color)),
        ("supports_cct".to_string(), json!(cct)),
        // Hue transitions over the same field it dims with, so a fitting that cannot dim
        // cannot ramp either. 6553000ms is the v2 maximum.
        ("supports_ramp".to_string(), json!(dimmer)),
        ("ramp_rate_max_ms".to_string(), json!(6_553_000u32)),
    ]
    .into_iter()
    .collect();

    if cct {
        // The whites this one actually spans, which is not the same on every model — a
        // filament bulb stops well short of 6500K, and offering the rest of the strip is
        // offering a color it will silently clamp. Mirek is reciprocal, so the minimum mirek
        // is the *warmest* number of kelvin and the two swap over.
        let mirek = |at: &str| {
            light
                .pointer(&format!("/color_temperature/mirek_schema/{at}"))
                .and_then(Value::as_f64)
                .filter(|m| *m > 0.0)
                .map(|m| (1_000_000.0 / m).round() as u32)
        };
        caps.insert(
            "cct_min".into(),
            json!(mirek("mirek_maximum").unwrap_or(2000)),
        );
        caps.insert(
            "cct_max".into(),
            json!(mirek("mirek_minimum").unwrap_or(6500)),
        );
    }
    caps
}

/// What to call this light's shape in a list somebody is reading.
fn bulb_shape(light: &Value) -> &'static str {
    let has = |name: &str| light.get(name).and_then(Value::as_object).is_some();
    match (has("color"), has("color_temperature"), has("dimming")) {
        (true, true, _) => "color and tunable white",
        (true, false, _) => "color",
        (false, true, _) => "tunable white",
        (false, false, true) => "dimmable",
        (false, false, false) => "on/off",
    }
}

#[cfg(test)]
mod bulb_capability_tests {
    use super::*;

    fn request_body(calls: &[HostCall]) -> Value {
        let HostCall::Http(request) = &calls[0] else {
            panic!("first call was not an HTTP request")
        };
        serde_json::from_str(request.body.as_deref().expect("request body")).unwrap()
    }

    fn bulb() -> Instance {
        let mut inst = Instance::new(1);
        inst.properties
            .insert("Bridge address".into(), json!("192.0.2.1"));
        inst.properties
            .insert("Application key".into(), json!("key"));
        inst.properties.insert("Light id".into(), json!("light"));
        inst
    }

    #[test]
    fn plain_on_is_power_only_while_set_level_explicitly_sets_brightness() {
        let mut inst = bulb();
        inst.scratch.insert("level".into(), json!(23));

        let on = HueBulb.on_command(&mut inst, 1, "on", &Args::new());
        assert_eq!(request_body(&on), json!({ "on": { "on": true } }));

        let mut args = Args::new();
        args.insert("level".into(), json!(100));
        let level = HueBulb.on_command(&mut inst, 1, "set_level", &args);
        assert_eq!(
            request_body(&level),
            json!({ "on": { "on": true }, "dimming": { "brightness": 100.0 } })
        );
    }

    #[test]
    fn an_off_resource_remembers_the_level_hue_will_restore() {
        let mut inst = bulb();
        HueBulb::report(
            &mut inst,
            &json!({ "on": { "on": false }, "dimming": { "brightness": 17.0 } }),
        );

        assert_eq!(inst.scratch.get("level"), Some(&json!(17)));
        assert_eq!(inst.scratch.get("on"), Some(&json!(false)));
        let on = HueBulb.on_command(&mut inst, 1, "on", &Args::new());
        assert_eq!(request_body(&on), json!({ "on": { "on": true } }));
    }

    #[test]
    fn a_white_light_is_not_offered_color() {
        let white = bulb_capabilities(&json!({ "dimming": {} }));
        assert_eq!(white["dimmer"], json!(true));
        assert_eq!(white["supports_color"], json!(false));
        assert_eq!(white["supports_cct"], json!(false));
        assert_eq!(bulb_shape(&json!({ "dimming": {} })), "dimmable");

        let color = bulb_capabilities(&json!({ "dimming": {}, "color": {} }));
        assert_eq!(color["supports_color"], json!(true));
    }

    #[test]
    fn absent_or_null_features_are_not_capabilities() {
        // A socket with a lamp in it. Nothing but on and off, and a control surface that
        // offers a brightness slider for it is a slider that does nothing.
        let on_off = bulb_capabilities(
            &json!({ "color": null, "color_temperature": null, "dimming": null }),
        );
        assert_eq!(on_off["dimmer"], json!(false));
        assert_eq!(on_off["supports_color"], json!(false));
        assert_eq!(on_off["supports_ramp"], json!(false));

        // The shape a real warm-white resource uses: the keys are present in a generic
        // response, but `color: null` is not a gamut. The concrete temperature object is the
        // evidence, so this must never come out as a color light.
        let warm_white = json!({
            "dimming": { "brightness": 42.0 },
            "color_temperature": { "mirek": 366, "mirek_valid": true },
            "color": null
        });
        let caps = bulb_capabilities(&warm_white);
        assert_eq!(caps["supports_color"], json!(false));
        assert_eq!(caps["supports_cct"], json!(true));
        assert_eq!(bulb_shape(&warm_white), "tunable white");
    }

    #[test]
    fn the_whites_offered_are_the_ones_this_fitting_spans() {
        // Mirek is reciprocal: the smallest mirek is the coolest white, so the two ends swap
        // over. Read the wrong way round a filament bulb advertises 6500K at the warm end and
        // every white it is asked for is clamped to something else.
        let filament = json!({
            "dimming": {},
            "color_temperature": { "mirek_schema": { "mirek_minimum": 222, "mirek_maximum": 454 } }
        });
        let caps = bulb_capabilities(&filament);
        assert_eq!(caps["cct_min"], json!(2203), "the warmest it goes");
        assert_eq!(caps["cct_max"], json!(4505), "and the coolest");

        // A bridge that did not say gets the range Hue's own range spans.
        let quiet = bulb_capabilities(&json!({ "dimming": {}, "color_temperature": {} }));
        assert_eq!(quiet["cct_min"], json!(2000));
        assert_eq!(quiet["cct_max"], json!(6500));
    }
}


#[cfg(test)]
mod color_round_trip {
    use super::*;

    /// What goes out has to come back. The bridge answers in CIE xy and the contract is degrees
    /// and percent, so without the conversion a lamp reported `hue = 0.2858` where the contract
    /// means a number up to 360 — and every surface reading color drew the wrong one.
    #[test]
    fn a_color_survives_the_trip_through_xy() {
        for (hue, sat) in [(0.0, 100.0), (120.0, 100.0), (240.0, 100.0), (30.0, 60.0), (200.0, 45.0)] {
            let (x, y) = hs_to_xy(hue, sat);
            let (back_hue, back_sat) = xy_to_hs(x, y);
            // Circular distance: 0 and 359 are one degree apart, not 359.
            let apart = (back_hue - hue).abs();
            let apart = apart.min(360.0 - apart);
            assert!(apart < 2.0, "hue {hue} came back as {back_hue}");
            assert!((back_sat - sat).abs() < 6.0, "sat {sat} came back as {back_sat}");
        }
    }

    /// The reading a real bulb gave, which used to arrive as the hue itself.
    #[test]
    fn a_bridge_reading_becomes_degrees_and_percent() {
        let mut inst = Instance::default();
        inst.properties.insert("Bridge address".into(), json!("10.0.0.2"));
        inst.properties.insert("Application key".into(), json!("k"));
        inst.properties.insert("Light id".into(), json!("l1"));
        let light = json!({ "color": { "xy": { "x": 0.2858, "y": 0.3083 } } });

        let said = HueBulb::report(&mut inst, &light);
        let color = said
            .iter()
            .find_map(|c| match c {
                HostCall::Notify { name, args, .. } if name == "color_changed" => Some(args.clone()),
                _ => None,
            })
            .expect("reports a color");

        let hue = color.get("hue").and_then(Value::as_f64).unwrap();
        let sat = color.get("sat").and_then(Value::as_f64).unwrap();
        assert!((0.0..=360.0).contains(&hue), "hue out of range: {hue}");
        assert!((0.0..=100.0).contains(&sat), "sat out of range: {sat}");
        assert!(hue > 1.0, "a blue-ish xy is not hue 0.2858 — got {hue}");
    }
}

#[cfg(test)]
mod grouped_zone_tests {
    use super::*;

    const ZONE: &str = "11111111-1111-1111-1111-111111111111";
    const GROUPED: &str = "22222222-2222-2222-2222-222222222222";

    fn bridge() -> Instance {
        let mut inst = Instance::new(10);
        inst.properties
            .insert("Bridge address".into(), json!("192.0.2.1"));
        inst.properties
            .insert("Application key".into(), json!("key"));
        inst.scratch.insert(HUE_BRIDGE_ID.into(), json!("BRIDGE-A"));
        inst.scratch.insert(
            HUE_ZONES.into(),
            json!([{
                "id": ZONE,
                "type": "zone",
                "metadata": { "name": "Upstairs" },
                "children": [
                    { "rid": "light-a", "rtype": "light" },
                    { "rid": "light-b", "rtype": "light" }
                ],
                "services": [{ "rid": GROUPED, "rtype": "grouped_light" }]
            }]),
        );
        inst
    }

    fn request(operation: GroupOperation) -> GroupRequest {
        let member = |device, id: &str| {
            let mut instance = Instance::new(device);
            instance.properties.insert("Light id".into(), json!(id));
            instance.scratch.insert("level".into(), json!(42));
            instance.scratch.insert("on".into(), json!(true));
            GroupMember {
                device,
                proxy: 1,
                instance,
                state: Args::new(),
            }
        };
        GroupRequest {
            group: 30,
            name: "Landing".into(),
            state: Args::new(),
            members: vec![member(20, "light-a"), member(21, "light-b")],
            operation,
        }
    }

    fn http(response: &GroupResponse) -> &HttpRequest {
        let HostCall::Http(request) = &response.calls[0] else {
            panic!("expected one HTTP call")
        };
        request
    }

    #[test]
    fn an_exact_existing_zone_is_borrowed_and_only_its_grouped_light_is_commanded() {
        let mut inst = bridge();
        let linked = HueBulb.on_group(
            &mut inst,
            &request(GroupOperation::Link {
                resource: ZONE.into(),
            }),
        );
        assert_eq!(linked.disposition, GroupDisposition::Handled);
        assert_eq!(
            group_link(&inst, 30).unwrap()["ownership"],
            json!("external")
        );

        let command = HueBulb.on_group(
            &mut inst,
            &request(GroupOperation::Command {
                command: "off".into(),
                args: Args::new(),
            }),
        );
        assert_eq!(command.disposition, GroupDisposition::Handled);
        assert_eq!(command.calls.len(), 1);
        assert_eq!(command.members.len(), 2);
        assert!(
            http(&command)
                .url
                .ends_with(&format!("/grouped_light/{GROUPED}"))
        );
        assert!(!http(&command).url.contains("/resource/zone/"));

        let sync = HueBulb.on_group(&mut inst, &request(GroupOperation::Synchronize));
        assert_eq!(sync.disposition, GroupDisposition::Refused);
        assert!(
            sync.calls.is_empty(),
            "a borrowed zone must never be written"
        );
    }

    #[test]
    fn changed_external_membership_falls_back_without_sending_a_group_request() {
        let mut inst = bridge();
        HueBulb.on_group(
            &mut inst,
            &request(GroupOperation::Link {
                resource: ZONE.into(),
            }),
        );
        inst.scratch
            .get_mut(HUE_ZONES)
            .and_then(Value::as_array_mut)
            .unwrap()[0]["children"] = json!([{ "rid": "light-a", "rtype": "light" }]);

        let response = HueBulb.on_group(
            &mut inst,
            &request(GroupOperation::Command {
                command: "on".into(),
                args: Args::new(),
            }),
        );
        assert_eq!(response.disposition, GroupDisposition::Refused);
        assert!(response.calls.is_empty());
        assert!(response.problem.unwrap().contains("membership changed"));
    }

    #[test]
    fn only_a_created_zone_gets_the_juno_ownership_record_and_can_be_synchronized() {
        let mut inst = bridge();
        inst.scratch.insert(HUE_ZONES.into(), json!([]));
        let create = HueBulb.on_group(&mut inst, &request(GroupOperation::Create));
        assert_eq!(create.disposition, GroupDisposition::Queued);
        assert!(http(&create).url.ends_with("/clip/v2/resource/zone"));
        assert_eq!(http(&create).method, "POST");
        let posted: Value = serde_json::from_str(http(&create).body.as_deref().unwrap()).unwrap();
        let created_name = posted.pointer("/metadata/name").unwrap().as_str().unwrap();
        assert!(created_name.contains("[Juno 0000001E]"));

        let mut created = Args::new();
        created.insert("method".into(), json!("POST"));
        created.insert(
            "url".into(),
            json!("https://192.0.2.1/clip/v2/resource/zone"),
        );
        created.insert("status".into(), json!(200));
        created.insert(
            "body".into(),
            json!({ "data": [{ "rid": ZONE, "rtype": "zone" }] }),
        );
        let refresh = HueBulb.on_event(&mut inst, 0, "http_response", &created);
        assert_eq!(refresh.len(), 1);

        let mut inventory = Args::new();
        inventory.insert("method".into(), json!("GET"));
        inventory.insert(
            "url".into(),
            json!("https://192.0.2.1/clip/v2/resource/zone"),
        );
        inventory.insert("status".into(), json!(200));
        inventory.insert(
            "body".into(),
            json!({ "data": [{
                "id": ZONE,
                "type": "zone",
                "metadata": { "name": created_name },
                "children": [
                    { "rid": "light-a", "rtype": "light" },
                    { "rid": "light-b", "rtype": "light" }
                ],
                "services": [{ "rid": GROUPED, "rtype": "grouped_light" }]
            }] }),
        );
        HueBulb.on_event(&mut inst, 0, "http_response", &inventory);
        assert_eq!(group_link(&inst, 30).unwrap()["ownership"], json!("juno"));

        let sync = HueBulb.on_group(&mut inst, &request(GroupOperation::Synchronize));
        assert_eq!(sync.disposition, GroupDisposition::Queued);
        assert_eq!(http(&sync).method, "PUT");
        assert!(http(&sync).url.ends_with(&format!("/resource/zone/{ZONE}")));
    }
}
