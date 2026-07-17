use std::io::{Read, Seek, SeekFrom};

const CENTRAL_CAP: u64 = 64 * 1024 * 1024;
const ZIP64_CAP: u64 = 64 * 1024;
const NAME_CAP: usize = 4 * 1024;
const COMPONENT_CAP: usize = 255;
const DEPTH_CAP: usize = 64;
const TOTAL_NAME_CAP: u64 = 32 * 1024 * 1024;
const TOTAL_COMPONENT_CAP: u64 = 1_000_000;
const ENTRY_CAP: u64 = 100_000;

#[derive(Clone, Copy)]
pub(super) struct ScanLimits {
    pub(super) max_entries: u64,
    pub(super) central_cap: u64,
    pub(super) zip64_cap: u64,
    pub(super) name_cap: usize,
    pub(super) component_cap: usize,
    pub(super) depth_cap: usize,
    pub(super) total_name_cap: u64,
    pub(super) total_component_cap: u64,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum ScanError {
    Fallback,
    Limit,
}
impl ScanLimits {
    pub(super) fn production(max_entries: u64) -> Self {
        Self {
            max_entries: max_entries.min(ENTRY_CAP),
            central_cap: CENTRAL_CAP,
            zip64_cap: ZIP64_CAP,
            name_cap: NAME_CAP,
            component_cap: COMPONENT_CAP,
            depth_cap: DEPTH_CAP,
            total_name_cap: TOTAL_NAME_CAP,
            total_component_cap: TOTAL_COMPONENT_CAP,
        }
    }
}

#[derive(Debug)]
pub(super) struct ScanResult {
    pub(super) count: u64,
    pub(super) central_start: u64,
}

pub(super) fn scan<R: Read + Seek>(
    reader: &mut R,
    limits: ScanLimits,
) -> Result<ScanResult, ScanError> {
    if limits.max_entries == 0
        || limits.central_cap == 0
        || limits.zip64_cap < 44
        || limits.name_cap == 0
        || limits.component_cap == 0
        || limits.depth_cap == 0
        || limits.total_name_cap == 0
        || limits.total_component_cap == 0
    {
        return Err(ScanError::Limit);
    }
    let end = reader.seek(SeekFrom::End(0)).map_err(|_| bad())?;
    let tail_start = end.saturating_sub(22 + 65_535);
    let mut tail = vec![0; usize::try_from(end - tail_start).map_err(|_| ScanError::Fallback)?];
    at(reader, tail_start, &mut tail)?;
    let eocd_at = tail
        .windows(4)
        .enumerate()
        .rev()
        .find_map(|(i, x)| {
            if x != b"PK\x05\x06" || i + 22 > tail.len() {
                return None;
            }
            let n = u16at(&tail, i + 20) as usize;
            (i + 22 + n == tail.len()).then_some(tail_start + i as u64)
        })
        .ok_or_else(bad)?;
    let e = &tail[usize::try_from(eocd_at - tail_start).map_err(|_| bad())?..];
    if u16at(e, 4) != 0 || u16at(e, 6) != 0 || u16at(e, 8) != u16at(e, 10) {
        return Err(bad());
    }
    let count_saturated = u16at(e, 8) == u16::MAX;
    let size_or_offset_saturated = u32at(e, 12) == u32::MAX || u32at(e, 16) == u32::MAX;
    let (count, central_size, central_start, central_end_expected) = directory_values(
        reader,
        e,
        eocd_at,
        end,
        limits.zip64_cap,
        count_saturated,
        size_or_offset_saturated,
    )?;
    if count > limits.max_entries.min(ENTRY_CAP) || central_size > limits.central_cap {
        return Err(ScanError::Limit);
    }
    let central_end = central_start.checked_add(central_size).ok_or_else(bad)?;
    if central_end != central_end_expected || central_end > end {
        return Err(bad());
    }
    let mut cursor = central_start;
    let mut names = 0u64;
    let mut components = 0u64;
    let mut ranges = Vec::with_capacity(usize::try_from(count).map_err(|_| bad())?);
    for _ in 0..count {
        let mut h = [0; 46];
        take(reader, &mut cursor, central_end, &mut h)?;
        if &h[..4] != b"PK\x01\x02" || !matches!(u16at(&h, 34), 0 | u16::MAX) {
            return Err(bad());
        }
        let flags = u16at(&h, 8);
        let method = u16at(&h, 10);
        if flags & 0x0009 != 0 || !matches!(method, 0 | 8) {
            return Err(bad());
        }
        let nl = u16at(&h, 28) as usize;
        let xl = u16at(&h, 30) as usize;
        let cl = u16at(&h, 32) as usize;
        if nl > limits.name_cap {
            return Err(ScanError::Limit);
        }
        names = names
            .checked_add(nl as u64)
            .filter(|v| *v <= limits.total_name_cap)
            .ok_or(ScanError::Limit)?;
        let mut name = vec![0; nl];
        take(reader, &mut cursor, central_end, &mut name)?;
        validate_name(&name, limits, &mut components)?;
        let mut extra = vec![0; xl];
        take(reader, &mut cursor, central_end, &mut extra)?;
        skip(reader, &mut cursor, central_end, cl as u64)?;
        let (compressed, local) = zip64_values(&h, &extra)?;
        if local
            .checked_add(30)
            .filter(|end| *end <= central_start)
            .is_none()
        {
            return Err(bad());
        }
        let mut local_h = [0; 30];
        at(reader, local, &mut local_h)?;
        if &local_h[..4] != b"PK\x03\x04" || u16at(&local_h, 8) != method {
            return Err(bad());
        }
        let local_flags = u16at(&local_h, 6);
        if local_flags & 0x0009 != 0 {
            return Err(bad());
        }
        let lnl = u16at(&local_h, 26) as usize;
        let lxl = u16at(&local_h, 28) as u64;
        if lnl > limits.name_cap {
            return Err(ScanError::Limit);
        }
        let name_at = local.checked_add(30).ok_or_else(bad)?;
        let mut local_name = vec![0; lnl];
        at(reader, name_at, &mut local_name)?;
        if local_name != name {
            return Err(bad());
        }
        let data = name_at
            .checked_add(lnl as u64)
            .and_then(|v| v.checked_add(lxl))
            .ok_or_else(bad)?;
        let data_end = data.checked_add(compressed).ok_or_else(bad)?;
        if data_end > central_start {
            return Err(bad());
        }
        ranges.push((local, data_end));
    }
    if cursor != central_end {
        return Err(bad());
    }
    // Sorted preflight replaces zip's O(n²) has_overlapping_files check.
    ranges.sort_unstable();
    if count == 0 && central_start != 0 || count != 0 && ranges.iter().map(|r| r.0).min() != Some(0)
    {
        return Err(bad());
    }
    if ranges.windows(2).any(|r| r[0].1 > r[1].0) {
        return Err(bad());
    }
    Ok(ScanResult {
        count,
        central_start,
    })
}

fn directory_values<R: Read + Seek>(
    reader: &mut R,
    e: &[u8],
    eocd_at: u64,
    end: u64,
    zip64_cap: u64,
    count_saturated: bool,
    size_or_offset_saturated: bool,
) -> Result<(u64, u64, u64, u64), ScanError> {
    let zip32 = || {
        (
            u16at(e, 8) as u64,
            u32at(e, 12) as u64,
            u32at(e, 16) as u64,
            eocd_at,
        )
    };
    if size_or_offset_saturated {
        zip64(reader, eocd_at, end, zip64_cap)
    } else if count_saturated {
        // 0xffff is valid ZIP32 unless the complete ZIP64 record validates.
        match zip64(reader, eocd_at, end, zip64_cap) {
            Ok(values) => Ok(values),
            Err(ScanError::Fallback) => Ok(zip32()),
            Err(ScanError::Limit) => Err(ScanError::Limit),
        }
    } else {
        Ok(zip32())
    }
}

fn zip64<R: Read + Seek>(
    r: &mut R,
    eocd: u64,
    end: u64,
    cap: u64,
) -> Result<(u64, u64, u64, u64), ScanError> {
    if eocd < 20 {
        return Err(bad());
    }
    let mut l = [0; 20];
    at(r, eocd - 20, &mut l)?;
    if &l[..4] != b"PK\x06\x07" || u32at(&l, 4) != 0 || u32at(&l, 16) != 1 {
        return Err(bad());
    }
    let pos = u64at(&l, 8);
    let mut fixed = [0; 12];
    at(r, pos, &mut fixed)?;
    if &fixed[..4] != b"PK\x06\x06" {
        return Err(bad());
    }
    let payload = u64at(&fixed, 4);
    if payload < 44 {
        return Err(ScanError::Fallback);
    }
    if payload > cap {
        return Err(ScanError::Limit);
    }
    let record_end = pos
        .checked_add(12)
        .and_then(|v| v.checked_add(payload))
        .ok_or_else(bad)?;
    if record_end != eocd - 20 || record_end > end {
        return Err(bad());
    }
    let mut p = vec![0; usize::try_from(payload).map_err(|_| ScanError::Fallback)?];
    at(r, pos + 12, &mut p)?;
    if u32at(&p, 4) != 0 || u32at(&p, 8) != 0 || u64at(&p, 12) != u64at(&p, 20) {
        return Err(bad());
    }
    Ok((u64at(&p, 12), u64at(&p, 28), u64at(&p, 36), pos))
}

fn zip64_values(h: &[u8; 46], extra: &[u8]) -> Result<(u64, u64), ScanError> {
    let mut need = [
        u32at(h, 24) == u32::MAX,
        u32at(h, 20) == u32::MAX,
        u32at(h, 42) == u32::MAX,
        u16at(h, 34) == u16::MAX,
    ];
    let mut values = [
        u32at(h, 24) as u64,
        u32at(h, 20) as u64,
        u32at(h, 42) as u64,
        u16at(h, 34) as u64,
    ];
    let mut i = 0;
    let mut found = false;
    while i < extra.len() {
        if i + 4 > extra.len() {
            return Err(bad());
        }
        let id = u16at(extra, i);
        let n = u16at(extra, i + 2) as usize;
        i += 4;
        if i + n > extra.len() {
            return Err(bad());
        }
        if id == 1 {
            if found {
                return Err(bad());
            }
            found = true;
            let mut j = i;
            for k in 0..4 {
                if need[k] {
                    let width = if k == 3 { 4 } else { 8 };
                    if j + width > i + n {
                        return Err(bad());
                    }
                    values[k] = if k == 3 {
                        u32at(extra, j) as u64
                    } else {
                        u64at(extra, j)
                    };
                    j += width;
                    need[k] = false;
                }
            }
        }
        i += n;
    }
    if need.iter().any(|v| *v) {
        return Err(bad());
    }
    if values[3] != 0 {
        return Err(bad());
    }
    Ok((values[1], values[2]))
}
fn validate_name(n: &[u8], l: ScanLimits, components: &mut u64) -> Result<(), ScanError> {
    if n.is_empty() || n.iter().any(|b| *b == b'\\' || *b < 0x20 || *b == 0x7f) {
        return Err(bad());
    }
    let mut parts = n.split(|b| *b == b'/').peekable();
    while let Some(p) = parts.next() {
        if p.is_empty() && parts.peek().is_none() {
            continue;
        }
        if p.len() > l.component_cap {
            return Err(ScanError::Limit);
        }
        if p.is_empty() || p == b"." || p == b".." {
            return Err(bad());
        }
        *components = components
            .checked_add(1)
            .filter(|v| *v <= l.total_component_cap)
            .ok_or(ScanError::Limit)?;
    }
    let depth = n.split(|b| *b == b'/').filter(|p| !p.is_empty()).count();
    if depth > l.depth_cap {
        return Err(ScanError::Limit);
    }
    Ok(())
}
fn sentinel(e: &[u8]) -> bool {
    u16at(e, 8) == u16::MAX || u32at(e, 12) == u32::MAX || u32at(e, 16) == u32::MAX
}
fn bad() -> ScanError {
    ScanError::Fallback
}
fn u16at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}
fn u32at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}
fn u64at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}
fn at<R: Read + Seek>(r: &mut R, pos: u64, b: &mut [u8]) -> Result<(), ScanError> {
    r.seek(SeekFrom::Start(pos))
        .map_err(|_| ScanError::Fallback)?;
    r.read_exact(b).map_err(|_| ScanError::Fallback)
}
fn take<R: Read + Seek>(
    r: &mut R,
    cursor: &mut u64,
    end: u64,
    b: &mut [u8],
) -> Result<(), ScanError> {
    let next = cursor
        .checked_add(b.len() as u64)
        .filter(|v| *v <= end)
        .ok_or_else(bad)?;
    at(r, *cursor, b)?;
    *cursor = next;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn limits() -> ScanLimits {
        ScanLimits {
            max_entries: 4,
            central_cap: 4096,
            zip64_cap: 64,
            name_cap: 16,
            component_cap: 8,
            depth_cap: 3,
            total_name_cap: 32,
            total_component_cap: 8,
        }
    }
    fn zip(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut b = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let local = b.len() as u32;
            b.extend(b"PK\x03\x04");
            b.extend([20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            b.extend([0; 4]);
            b.extend((data.len() as u32).to_le_bytes());
            b.extend((data.len() as u32).to_le_bytes());
            b.extend((name.len() as u16).to_le_bytes());
            b.extend(0u16.to_le_bytes());
            b.extend(*name);
            b.extend(*data);
            central.extend(b"PK\x01\x02");
            central.extend([20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            central.extend([0; 4]);
            central.extend((data.len() as u32).to_le_bytes());
            central.extend((data.len() as u32).to_le_bytes());
            central.extend((name.len() as u16).to_le_bytes());
            central.extend([0; 12]);
            central.extend(local.to_le_bytes());
            central.extend(*name);
        }
        let start = b.len() as u32;
        let size = central.len() as u32;
        b.extend(central);
        b.extend(b"PK\x05\x06");
        b.extend([0; 4]);
        b.extend((entries.len() as u16).to_le_bytes());
        b.extend((entries.len() as u16).to_le_bytes());
        b.extend(size.to_le_bytes());
        b.extend(start.to_le_bytes());
        b.extend(0u16.to_le_bytes());
        b
    }
    fn pos(b: &[u8], sig: &[u8; 4]) -> usize {
        b.windows(4).rposition(|x| x == sig).unwrap()
    }
    fn p16(b: &mut [u8], i: usize, n: u16) {
        b[i..i + 2].copy_from_slice(&n.to_le_bytes());
    }
    fn p32(b: &mut [u8], i: usize, n: u32) {
        b[i..i + 4].copy_from_slice(&n.to_le_bytes());
    }
    fn p64(b: &mut [u8], i: usize, n: u64) {
        b[i..i + 8].copy_from_slice(&n.to_le_bytes());
    }
    fn ok(b: Vec<u8>, l: ScanLimits) -> bool {
        scan(&mut Cursor::new(b), l).is_ok()
    }
    fn err(b: Vec<u8>, l: ScanLimits) -> ScanError {
        scan(&mut Cursor::new(b), l).unwrap_err()
    }
    fn zip64(mut b: Vec<u8>) -> Vec<u8> {
        let e = pos(&b, b"PK\x05\x06");
        let count = u16at(&b, e + 10) as u64;
        let size = u32at(&b, e + 12) as u64;
        let start = u32at(&b, e + 16) as u64;
        let mut r = b"PK\x06\x06".to_vec();
        r.extend(44u64.to_le_bytes());
        r.extend(45u16.to_le_bytes());
        r.extend(45u16.to_le_bytes());
        r.extend(0u32.to_le_bytes());
        r.extend(0u32.to_le_bytes());
        r.extend(count.to_le_bytes());
        r.extend(count.to_le_bytes());
        r.extend(size.to_le_bytes());
        r.extend(start.to_le_bytes());
        let mut l = b"PK\x06\x07".to_vec();
        l.extend(0u32.to_le_bytes());
        l.extend((e as u64).to_le_bytes());
        l.extend(1u32.to_le_bytes());
        p16(&mut b, e + 8, u16::MAX);
        p16(&mut b, e + 10, u16::MAX);
        p32(&mut b, e + 12, u32::MAX);
        p32(&mut b, e + 16, u32::MAX);
        b.splice(e..e, r.into_iter().chain(l));
        b
    }
    fn central(b: &[u8]) -> usize {
        b.windows(4).position(|x| x == b"PK\x01\x02").unwrap()
    }
    fn local(b: &[u8]) -> usize {
        b.windows(4).position(|x| x == b"PK\x03\x04").unwrap()
    }
    fn eocd(b: &[u8]) -> usize {
        pos(b, b"PK\x05\x06")
    }
    fn one() -> Vec<u8> {
        zip(&[(b"a", b"x")])
    }
    fn zip64_extra(need: [bool; 4], values: [u64; 4]) -> (Vec<u8>, Vec<u8>) {
        let mut h = [0; 46];
        let mut body = Vec::new();
        for (i, needed) in need.into_iter().enumerate() {
            if needed {
                match i {
                    0 => p32(&mut h, 24, u32::MAX),
                    1 => p32(&mut h, 20, u32::MAX),
                    2 => p32(&mut h, 42, u32::MAX),
                    _ => p16(&mut h, 34, u16::MAX),
                }
                if i == 3 {
                    body.extend((values[i] as u32).to_le_bytes())
                } else {
                    body.extend(values[i].to_le_bytes())
                }
            }
        }
        let mut extra = 1u16.to_le_bytes().to_vec();
        extra.extend((body.len() as u16).to_le_bytes());
        extra.extend(body);
        (h.to_vec(), extra)
    }
    fn h46(v: Vec<u8>) -> [u8; 46] {
        v.try_into().unwrap()
    }

    #[test]
    fn accepts_zip32() {
        assert!(ok(one(), limits()));
    }
    #[test]
    fn accepts_zip64() {
        assert!(ok(zip64(one()), limits()));
    }
    #[test]
    fn accepts_zip32_count_65535_without_locator() {
        let entries: Vec<_> = (0..65_535)
            .map(|i| (format!("{i:05x}").into_bytes(), Vec::new()))
            .collect();
        let refs: Vec<_> = entries
            .iter()
            .map(|(name, data)| (name.as_slice(), data.as_slice()))
            .collect();
        let result = scan(
            &mut Cursor::new(zip(&refs)),
            ScanLimits::production(100_000),
        )
        .unwrap();
        assert_eq!(result.count, 65_535);
    }
    #[test]
    fn accepts_saturated_count_with_valid_zip64_locator() {
        assert!(ok(zip64(one()), limits()));
    }
    #[test]
    fn invalid_zip64_locator_does_not_hijack_exact_zip32_count() {
        let mut locator = b"PK\x06\x07".to_vec();
        locator.extend(0u32.to_le_bytes());
        locator.extend(0u64.to_le_bytes());
        locator.extend(1u32.to_le_bytes());
        let mut eocd = [0; 22];
        p16(&mut eocd, 8, u16::MAX);
        p16(&mut eocd, 10, u16::MAX);
        p32(&mut eocd, 12, 46);
        p32(&mut eocd, 16, 30);
        assert_eq!(
            directory_values(
                &mut Cursor::new(locator),
                &eocd,
                20,
                42,
                limits().zip64_cap,
                true,
                false,
            ),
            Ok((65_535, 46, 30, 20))
        );
    }
    #[test]
    fn saturated_count_zip64_limit_does_not_fall_back_to_zip32() {
        let mut b = zip64(one());
        let e = eocd(&b);
        let record = e - 76;
        let size = u64at(&b, record + 48) as u32;
        let start = u64at(&b, record + 56) as u32;
        p32(&mut b, e + 12, size);
        p32(&mut b, e + 16, start);
        assert_eq!(
            directory_values(
                &mut Cursor::new(&b),
                &b[e..e + 22],
                e as u64,
                b.len() as u64,
                43,
                true,
                false,
            ),
            Err(ScanError::Limit)
        );
    }
    #[test]
    fn rejects_saturated_size_or_offset_without_locator() {
        for offset in [12, 16] {
            let mut b = one();
            let e = eocd(&b);
            p32(&mut b, e + offset, u32::MAX);
            assert!(!ok(b, limits()));
        }
    }
    #[test]
    fn accepts_eocd_signature_in_comment() {
        let mut b = one();
        let e = eocd(&b);
        p16(&mut b, e + 20, 4);
        b.extend(b"PK\x05\x06");
        assert!(ok(b.clone(), limits()));
    }
    #[test]
    fn rejects_eocd_disk_number() {
        let mut b = one();
        let e = eocd(&b);
        p16(&mut b, e + 4, 1);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_eocd_central_disk() {
        let mut b = one();
        let e = eocd(&b);
        p16(&mut b, e + 6, 1);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_eocd_per_disk_count_mismatch() {
        let mut b = one();
        let e = eocd(&b);
        p16(&mut b, e + 8, 0);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_zip64_bad_locator_signature() {
        let mut b = zip64(one());
        let e = eocd(&b);
        p32(&mut b, e - 20, 0);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_zip64_locator_disk() {
        let mut b = zip64(one());
        let e = eocd(&b);
        p32(&mut b, e - 16, 1);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_zip64_locator_count() {
        let mut b = zip64(one());
        let e = eocd(&b);
        p32(&mut b, e - 4, 2);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn zip64_payload_boundaries() {
        let b = zip64(one());
        let e = eocd(&b);
        let r = e - 76;
        let mut short = b.clone();
        p64(&mut short, r + 4, 43);
        assert_eq!(err(short, limits()), ScanError::Fallback);
        let mut payload64 = b;
        p64(&mut payload64, r + 4, 64);
        payload64.splice(r + 56..r + 56, [0; 20]);
        let mut exact = limits();
        exact.zip64_cap = 64;
        assert!(ok(payload64.clone(), exact));
        exact.zip64_cap = 63;
        assert_eq!(err(payload64, exact), ScanError::Limit);
    }
    #[test]
    fn rejects_zip64_record_end_mismatch() {
        let mut b = zip64(one());
        let e = eocd(&b);
        p64(&mut b, e - 12, 0);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_zip64_central_end_mismatch() {
        let mut b = zip64(one());
        let e = eocd(&b);
        p64(&mut b, e - 76 + 48, 0);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_zip64_record_disk_number() {
        let mut b = zip64(one());
        let e = eocd(&b);
        p32(&mut b, e - 76 + 16, 1);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_zip64_record_central_disk() {
        let mut b = zip64(one());
        let e = eocd(&b);
        p32(&mut b, e - 76 + 20, 1);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_zip64_record_count_mismatch() {
        let mut b = zip64(one());
        let e = eocd(&b);
        p64(&mut b, e - 76 + 24, 0);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn entry_limit_boundary() {
        let b = zip(&[(b"a", b""), (b"b", b"")]);
        let mut l = limits();
        l.max_entries = 2;
        assert!(ok(b.clone(), l));
        l.max_entries = 1;
        assert!(!ok(b, l));
    }
    #[test]
    fn central_cap_boundary() {
        let b = one();
        let mut l = limits();
        l.central_cap = 47;
        assert!(ok(b.clone(), l));
        l.central_cap = 46;
        assert!(!ok(b, l));
    }
    #[test]
    fn rejects_bad_central_fixed_header() {
        let mut b = one();
        let c = central(&b);
        p32(&mut b, c, 0);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_central_name_escape() {
        let mut b = one();
        let c = central(&b);
        p16(&mut b, c + 28, 2);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_central_extra_escape() {
        let mut b = one();
        let c = central(&b);
        p16(&mut b, c + 30, 1);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_central_comment_escape() {
        let mut b = one();
        let c = central(&b);
        p16(&mut b, c + 32, 1);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_central_offset_mismatch() {
        let mut b = one();
        let e = eocd(&b);
        p32(&mut b, e + 16, 1);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_central_size_mismatch() {
        let mut b = one();
        let e = eocd(&b);
        p32(&mut b, e + 12, 46);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn name_cap_boundary() {
        let mut l = limits();
        l.name_cap = 1;
        assert!(ok(one(), l));
        assert!(!ok(zip(&[(b"ab", b"")]), l));
    }
    #[test]
    fn component_cap_boundary() {
        let mut l = limits();
        l.name_cap = 16;
        l.component_cap = 1;
        assert!(ok(one(), l));
        assert!(!ok(zip(&[(b"ab", b"")]), l));
    }
    #[test]
    fn depth_cap_boundary() {
        let mut l = limits();
        l.name_cap = 16;
        l.component_cap = 16;
        l.depth_cap = 1;
        assert!(ok(zip(&[(b"a", b"")]), l));
        assert!(!ok(zip(&[(b"a/b", b"")]), l));
    }
    #[test]
    fn total_name_and_component_limits() {
        let b = zip(&[(b"a", b""), (b"b", b"")]);
        let mut l = limits();
        l.total_name_cap = 2;
        l.total_component_cap = 2;
        assert!(ok(b.clone(), l));
        l.total_name_cap = 1;
        assert!(!ok(b.clone(), l));
        l.total_name_cap = 2;
        l.total_component_cap = 1;
        assert!(!ok(b, l));
    }
    #[test]
    fn rejects_empty_name() {
        assert!(!ok(zip(&[(b"", b"")]), limits()));
    }
    #[test]
    fn rejects_leading_slash() {
        assert!(!ok(zip(&[(b"/a", b"")]), limits()));
    }
    #[test]
    fn rejects_internal_or_extra_trailing_slash() {
        assert!(!ok(zip(&[(b"a//b", b"")]), limits()));
        assert!(!ok(zip(&[(b"a///", b"")]), limits()));
    }
    #[test]
    fn rejects_dot_name() {
        assert!(!ok(zip(&[(b".", b"")]), limits()));
    }
    #[test]
    fn rejects_dotdot_name() {
        assert!(!ok(zip(&[(b"..", b"")]), limits()));
    }
    #[test]
    fn rejects_interior_dot_segment() {
        assert!(!ok(zip(&[(b"a/./b", b"")]), limits()));
    }
    #[test]
    fn rejects_interior_dotdot_segment() {
        assert!(!ok(zip(&[(b"a/../b", b"")]), limits()));
    }
    #[test]
    fn rejects_trailing_dot_segment() {
        assert!(!ok(zip(&[(b"a/.", b"")]), limits()));
    }
    #[test]
    fn rejects_trailing_dotdot_segment() {
        assert!(!ok(zip(&[(b"a/..", b"")]), limits()));
    }
    #[test]
    fn rejects_backslash_name() {
        assert!(!ok(zip(&[(b"a\\b", b"")]), limits()));
    }
    #[test]
    fn rejects_control_name() {
        assert!(!ok(zip(&[(b"a\x01", b"")]), limits()));
    }
    #[test]
    fn rejects_central_method() {
        let mut b = one();
        let c = central(&b);
        p16(&mut b, c + 10, 9);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_central_encryption() {
        let mut b = one();
        let c = central(&b);
        p16(&mut b, c + 8, 1);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_central_data_descriptor() {
        let mut b = one();
        let c = central(&b);
        p16(&mut b, c + 8, 0x0008);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_local_method() {
        let mut b = one();
        let l = local(&b);
        p16(&mut b, l + 8, 8);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_local_encryption() {
        let mut b = one();
        let l = local(&b);
        p16(&mut b, l + 6, 1);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_local_data_descriptor() {
        let mut b = one();
        let l = local(&b);
        p16(&mut b, l + 6, 0x0008);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_local_raw_name() {
        let mut b = one();
        let l = local(&b);
        b[l + 30] = b'b';
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_local_crossing_central() {
        let mut b = one();
        let c = central(&b);
        p32(&mut b, c + 42, (c - 29) as u32);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_trailing_bytes() {
        let mut b = one();
        b.push(0);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_adjusted_prepend() {
        let mut b = one();
        let c = central(&b);
        let e = eocd(&b);
        p32(&mut b, c + 42, 4);
        p32(&mut b, e + 16, (c + 4) as u32);
        b.splice(0..0, b"junk".iter().copied());
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_overlapping_locals() {
        let mut b = zip(&[(b"a", b"x"), (b"a", b"y")]);
        let c = central(&b);
        p32(&mut b, c + 47 + 42, 0);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn rejects_duplicate_zero_length_local() {
        let mut b = zip(&[(b"a", b""), (b"a", b"")]);
        let c = central(&b);
        p32(&mut b, c + 47 + 42, 0);
        assert!(!ok(b, limits()));
    }
    #[test]
    fn zip64_extra_values_and_valid_disk_zero() {
        let (h, x) = zip64_extra([true; 4], [1, 2, 3, 0]);
        assert_eq!(zip64_values(&h46(h), &x), Ok((2, 3)));
        let (h, x) = zip64_extra([false; 4], [0; 4]);
        assert_eq!(zip64_values(&h46(h), &x), Ok((0, 0)));
    }
    #[test]
    fn zip64_extra_rejects_missing_uncompressed() {
        let (h, _) = zip64_extra([true, false, false, false], [1, 0, 0, 0]);
        assert!(zip64_values(&h46(h), &[]).is_err());
    }
    #[test]
    fn zip64_extra_rejects_missing_compressed() {
        let (h, _) = zip64_extra([false, true, false, false], [0, 1, 0, 0]);
        assert!(zip64_values(&h46(h), &[]).is_err());
    }
    #[test]
    fn zip64_extra_rejects_missing_local_offset() {
        let (h, _) = zip64_extra([false, false, true, false], [0, 0, 1, 0]);
        assert!(zip64_values(&h46(h), &[]).is_err());
    }
    #[test]
    fn zip64_extra_rejects_missing_disk() {
        let (h, _) = zip64_extra([false, false, false, true], [0, 0, 0, 0]);
        assert!(zip64_values(&h46(h), &[]).is_err());
    }
    #[test]
    fn zip64_extra_rejects_truncated() {
        let (h, mut x) = zip64_extra([true; 4], [1, 2, 3, 0]);
        x.truncate(x.len() - 1);
        assert!(zip64_values(&h46(h.clone()), &x).is_err());
    }
    #[test]
    fn zip64_extra_rejects_nonzero_disk() {
        let (h, x) = zip64_extra([true; 4], [1, 2, 3, 1]);
        assert!(zip64_values(&h46(h.clone()), &x).is_err());
    }
    #[test]
    fn zip64_extra_rejects_duplicate_id() {
        let (h, x) = zip64_extra([true; 4], [1, 2, 3, 0]);
        let mut twice = x.clone();
        twice.extend(x);
        assert!(zip64_values(&h46(h), &twice).is_err());
    }
    #[test]
    fn production_limits_cap_entries_without_allocating() {
        assert_eq!(ScanLimits::production(0).max_entries, 0);
        assert_eq!(ScanLimits::production(u64::MAX).max_entries, ENTRY_CAP);
    }
    #[test]
    fn scanner_errors_classify_fallbacks_and_limits() {
        let mut unsupported = one();
        let central_offset = central(&unsupported);
        p16(&mut unsupported, central_offset + 10, 9);
        assert_eq!(err(unsupported, limits()), ScanError::Fallback);
        let mut descriptor = one();
        let central_offset = central(&descriptor);
        p16(&mut descriptor, central_offset + 8, 8);
        assert_eq!(err(descriptor, limits()), ScanError::Fallback);
        let mut truncated = one();
        truncated.truncate(truncated.len() - 1);
        assert_eq!(err(truncated, limits()), ScanError::Fallback);

        let entries = zip(&[(b"a", b""), (b"b", b"")]);
        let mut l = limits();
        l.max_entries = 1;
        assert_eq!(err(entries, l), ScanError::Limit);
        let mut l = limits();
        l.central_cap = 46;
        assert_eq!(err(one(), l), ScanError::Limit);
        let mut l = limits();
        l.name_cap = 1;
        assert_eq!(err(zip(&[(b"ab", b"")]), l), ScanError::Limit);
        let mut l = limits();
        l.component_cap = 1;
        assert_eq!(err(zip(&[(b"ab", b"")]), l), ScanError::Limit);
        let mut l = limits();
        l.depth_cap = 1;
        assert_eq!(err(zip(&[(b"a/b", b"")]), l), ScanError::Limit);
        let mut l = limits();
        l.total_name_cap = 1;
        assert_eq!(err(zip(&[(b"a", b""), (b"b", b"")]), l), ScanError::Limit);
        let mut l = limits();
        l.total_component_cap = 1;
        assert_eq!(err(zip(&[(b"a", b""), (b"b", b"")]), l), ScanError::Limit);
        let mut l = limits();
        l.zip64_cap = 43;
        assert_eq!(err(one(), l), ScanError::Limit);
    }
}
fn skip<R: Read + Seek>(r: &mut R, cursor: &mut u64, end: u64, n: u64) -> Result<(), ScanError> {
    let next = cursor
        .checked_add(n)
        .filter(|v| *v <= end)
        .ok_or_else(bad)?;
    r.seek(SeekFrom::Start(next)).map_err(|_| bad())?;
    *cursor = next;
    Ok(())
}
