use std::collections::{HashMap, HashSet};

use crate::error::{Result, VamanaError};

use super::{
    VamanaIndex, PORTABLE_IDS_MAGIC, PORTABLE_IDS_VERSION, PORTABLE_MAGIC, PORTABLE_VERSION,
};

pub(super) fn encode_portable_ids(
    index: &VamanaIndex,
    external_ids: &[(u32, String)],
) -> Result<Vec<u8>> {
    if external_ids.len() != index.live_count() {
        return Err(VamanaError::invalid_format(format!(
            "portable ID count {} != live count {}",
            external_ids.len(),
            index.live_count()
        )));
    }
    let mut ids = external_ids.to_vec();
    ids.sort_unstable_by_key(|(ordinal, _)| *ordinal);
    let mut seen_ordinals = HashSet::with_capacity(ids.len());
    let mut seen_ids = HashSet::with_capacity(ids.len());
    for (ordinal, id) in &ids {
        if *ordinal as usize >= index.num_vectors
            || index.is_tombstoned(*ordinal)
            || id.is_empty()
            || !seen_ordinals.insert(*ordinal)
            || !seen_ids.insert(id.as_str())
        {
            return Err(VamanaError::invalid_format(format!(
                "invalid portable ID entry for ordinal {ordinal}"
            )));
        }
    }
    for ordinal in 0..index.num_vectors as u32 {
        if !index.is_tombstoned(ordinal) && !seen_ordinals.contains(&ordinal) {
            return Err(VamanaError::invalid_format(format!(
                "missing portable ID for live ordinal {ordinal}"
            )));
        }
    }

    let mut buf = Vec::new();
    buf.extend_from_slice(PORTABLE_IDS_MAGIC);
    buf.extend_from_slice(&PORTABLE_IDS_VERSION.to_le_bytes());
    buf.extend_from_slice(&(ids.len() as u64).to_le_bytes());
    for (ordinal, id) in ids {
        let len = u32::try_from(id.len())
            .map_err(|_| VamanaError::invalid_format("portable ID length overflows u32".into()))?;
        buf.extend_from_slice(&ordinal.to_le_bytes());
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(id.as_bytes());
    }
    Ok(buf)
}

pub(super) fn parse_portable_ids(data: &[u8], index: &VamanaIndex) -> Result<Vec<(u32, String)>> {
    let mut offset = 0;
    if take_bytes(data, &mut offset, 8, "portable ID magic")? != PORTABLE_IDS_MAGIC {
        return Err(VamanaError::invalid_format(
            "portable_ids.bin magic mismatch".into(),
        ));
    }
    let version = read_u32(data, &mut offset, "portable ID version")?;
    if version != PORTABLE_IDS_VERSION {
        return Err(VamanaError::invalid_format(format!(
            "unsupported portable ID version {version}"
        )));
    }
    let count = usize::try_from(read_u64(data, &mut offset, "portable ID count")?)
        .map_err(|_| VamanaError::invalid_format("portable ID count overflows usize".into()))?;
    if count != index.live_count() {
        return Err(VamanaError::invalid_format(format!(
            "portable ID count {count} != live count {}",
            index.live_count()
        )));
    }

    let mut entries = Vec::with_capacity(count);
    let mut seen_ordinals = HashSet::with_capacity(count);
    let mut seen_ids = HashSet::with_capacity(count);
    let mut previous = None;
    for _ in 0..count {
        let ordinal = read_u32(data, &mut offset, "portable ID ordinal")?;
        let len = read_u32(data, &mut offset, "portable ID length")? as usize;
        let raw = take_bytes(data, &mut offset, len, "portable ID bytes")?;
        let id = std::str::from_utf8(raw)
            .map_err(|_| VamanaError::invalid_format("portable ID is not UTF-8".into()))?
            .to_owned();
        if previous.is_some_and(|previous| ordinal <= previous)
            || ordinal as usize >= index.num_vectors
            || index.is_tombstoned(ordinal)
            || id.is_empty()
            || !seen_ordinals.insert(ordinal)
            || !seen_ids.insert(id.clone())
        {
            return Err(VamanaError::invalid_format(format!(
                "invalid portable ID entry for ordinal {ordinal}"
            )));
        }
        previous = Some(ordinal);
        entries.push((ordinal, id));
    }
    if offset != data.len() {
        return Err(VamanaError::invalid_format(format!(
            "portable_ids.bin has {} trailing bytes",
            data.len() - offset
        )));
    }
    for ordinal in 0..index.num_vectors as u32 {
        if !index.is_tombstoned(ordinal) && !seen_ordinals.contains(&ordinal) {
            return Err(VamanaError::invalid_format(format!(
                "missing portable ID for live ordinal {ordinal}"
            )));
        }
    }
    Ok(entries)
}

pub(super) fn encode_portable_container(segments: &[(&str, Vec<u8>)]) -> Result<Vec<u8>> {
    let segment_count = u32::try_from(segments.len())
        .map_err(|_| VamanaError::invalid_format("portable segment count overflows u32".into()))?;
    let table_len = segments.iter().try_fold(16usize, |total, (name, _)| {
        total
            .checked_add(4 + name.len() + 8 + 8 + 32)
            .ok_or_else(|| VamanaError::invalid_format("portable table length overflow".into()))
    })?;
    let payload_len = segments.iter().try_fold(0usize, |total, (_, payload)| {
        total
            .checked_add(payload.len())
            .ok_or_else(|| VamanaError::invalid_format("portable payload length overflow".into()))
    })?;
    let mut buf = Vec::with_capacity(
        table_len
            .checked_add(payload_len)
            .ok_or_else(|| VamanaError::invalid_format("portable container overflow".into()))?,
    );
    buf.extend_from_slice(PORTABLE_MAGIC);
    buf.extend_from_slice(&PORTABLE_VERSION.to_le_bytes());
    buf.extend_from_slice(&segment_count.to_le_bytes());
    let mut payload_offset = table_len;
    for (name, payload) in segments {
        let name_len = u32::try_from(name.len())
            .map_err(|_| VamanaError::invalid_format("portable segment name too long".into()))?;
        buf.extend_from_slice(&name_len.to_le_bytes());
        buf.extend_from_slice(name.as_bytes());
        buf.extend_from_slice(&(payload_offset as u64).to_le_bytes());
        buf.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        buf.extend_from_slice(blake3::hash(payload).as_bytes());
        payload_offset += payload.len();
    }
    for (_, payload) in segments {
        buf.extend_from_slice(payload);
    }
    Ok(buf)
}

struct PortableSegment {
    offset: usize,
    len: usize,
    checksum: [u8; 32],
}

pub(super) fn parse_portable_container(data: &[u8]) -> Result<HashMap<String, &[u8]>> {
    let mut offset = 0;
    if take_bytes(data, &mut offset, 8, "portable magic")? != PORTABLE_MAGIC {
        return Err(VamanaError::invalid_format(
            "portable container magic mismatch".into(),
        ));
    }
    let version = read_u32(data, &mut offset, "portable version")?;
    if version != PORTABLE_VERSION {
        return Err(VamanaError::invalid_format(format!(
            "unsupported portable container version {version}"
        )));
    }
    let segment_count = read_u32(data, &mut offset, "portable segment count")? as usize;
    if !(4..=5).contains(&segment_count) {
        return Err(VamanaError::invalid_format(format!(
            "portable segment count {segment_count} is invalid"
        )));
    }

    let allowed = [
        "metadata.bin",
        "vectors.bin",
        "graph.bin",
        "lifecycle.bin",
        "portable_ids.bin",
    ];
    let mut table = HashMap::with_capacity(segment_count);
    for _ in 0..segment_count {
        let name_len = read_u32(data, &mut offset, "portable segment name length")? as usize;
        let name = std::str::from_utf8(take_bytes(
            data,
            &mut offset,
            name_len,
            "portable segment name",
        )?)
        .map_err(|_| VamanaError::invalid_format("portable segment name is not UTF-8".into()))?
        .to_owned();
        if !allowed.contains(&name.as_str()) {
            return Err(VamanaError::invalid_format(format!(
                "unknown portable segment {name}"
            )));
        }
        let payload_offset = usize::try_from(read_u64(data, &mut offset, "payload offset")?)
            .map_err(|_| VamanaError::invalid_format("payload offset overflows usize".into()))?;
        let payload_len = usize::try_from(read_u64(data, &mut offset, "payload length")?)
            .map_err(|_| VamanaError::invalid_format("payload length overflows usize".into()))?;
        let mut checksum = [0; 32];
        checksum.copy_from_slice(take_bytes(data, &mut offset, 32, "payload checksum")?);
        if table
            .insert(
                name.clone(),
                PortableSegment {
                    offset: payload_offset,
                    len: payload_len,
                    checksum,
                },
            )
            .is_some()
        {
            return Err(VamanaError::invalid_format(format!(
                "duplicate portable segment {name}"
            )));
        }
    }

    let table_end = offset;
    let mut ranges = Vec::with_capacity(table.len());
    for (name, segment) in &table {
        let end = segment.offset.checked_add(segment.len).ok_or_else(|| {
            VamanaError::invalid_format(format!("portable segment {name} range overflows"))
        })?;
        if segment.offset < table_end || end > data.len() {
            return Err(VamanaError::invalid_format(format!(
                "portable segment {name} range is out of bounds"
            )));
        }
        ranges.push((segment.offset, end, name));
    }
    ranges.sort_unstable_by_key(|(start, _, _)| *start);
    for pair in ranges.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(VamanaError::invalid_format(format!(
                "portable segments {} and {} overlap",
                pair[0].2, pair[1].2
            )));
        }
    }

    let mut payloads = HashMap::with_capacity(table.len());
    for (name, segment) in table {
        let payload = &data[segment.offset..segment.offset + segment.len];
        if blake3::hash(payload).as_bytes() != &segment.checksum {
            return Err(VamanaError::invalid_format(format!(
                "portable segment {name} checksum mismatch"
            )));
        }
        payloads.insert(name, payload);
    }
    Ok(payloads)
}

pub(super) fn required_segment<'a>(
    segments: &'a HashMap<String, &'a [u8]>,
    name: &str,
) -> Result<&'a [u8]> {
    segments
        .get(name)
        .copied()
        .ok_or_else(|| VamanaError::invalid_format(format!("missing portable segment {name}")))
}

fn take_bytes<'a>(data: &'a [u8], offset: &mut usize, len: usize, field: &str) -> Result<&'a [u8]> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| VamanaError::invalid_format(format!("{field} offset overflows")))?;
    if end > data.len() {
        return Err(VamanaError::invalid_format(format!(
            "portable container truncated at {field}"
        )));
    }
    let bytes = &data[*offset..end];
    *offset = end;
    Ok(bytes)
}

fn read_u32(data: &[u8], offset: &mut usize, field: &str) -> Result<u32> {
    Ok(u32::from_le_bytes(
        take_bytes(data, offset, 4, field)?
            .try_into()
            .expect("four-byte field"),
    ))
}

fn read_u64(data: &[u8], offset: &mut usize, field: &str) -> Result<u64> {
    Ok(u64::from_le_bytes(
        take_bytes(data, offset, 8, field)?
            .try_into()
            .expect("eight-byte field"),
    ))
}
