use super::*;

fn container(parts: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
    let header = 32 + 4 * parts.len();
    let mut offsets = Vec::new();
    let mut body = Vec::new();
    for (tag, data) in parts {
        offsets.push((header + body.len()) as u32);
        body.extend_from_slice(*tag);
        body.extend_from_slice(&(data.len() as u32).to_le_bytes());
        body.extend_from_slice(data);
    }
    let mut out = b"DXBC".to_vec();
    out.extend_from_slice(&[0; 16]);
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&((header + body.len()) as u32).to_le_bytes());
    out.extend_from_slice(&(parts.len() as u32).to_le_bytes());
    offsets.iter().for_each(|o| out.extend_from_slice(&o.to_le_bytes()));
    out.extend_from_slice(&body);
    out
}

fn sample_root() -> Root {
    let mut sampler = [0; 13];
    sampler[10] = 3;
    sampler[11] = 1;
    Root {
        flags: 0xc00,
        params: vec![
            Param::Table { visibility: 0, ranges: vec![[0, 1, 78, 0, 0, 0], [0, 1, 311, 0, 0, 1], [1, 1, 7, 0, 1, 2]] },
            Param::Table { visibility: 0, ranges: vec![[2, 1, 4, 2, 0, u32::MAX]] },
            Param::Words { kind: 3, visibility: 0, words: [1, 3, 0] },
            Param::Words { kind: 1, visibility: 0, words: [502, 0, 3] },
        ],
        samplers: vec![sampler],
    }
}

#[test]
fn a_written_root_reads_back_and_parses() {
    let root = sample_root();
    let bytes = write(&root);
    assert_eq!(read(&bytes).unwrap(), root);
    let signature = tuxbox_dxbc::rootsig::parse_rts0(&bytes).unwrap();
    assert_eq!(signature.params.len(), 4);
    assert_eq!(signature.static_samplers.len(), 1);
    // Header, then parameter headers from byte 24, then the first table's
    // header with its ranges right behind it.
    assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 24);
    assert_eq!(u32::from_le_bytes(bytes[32..36].try_into().unwrap()), 24 + 12 * 4);
    assert_eq!(u32::from_le_bytes(bytes[76..80].try_into().unwrap()), 24 + 12 * 4 + 8);
}

#[test]
fn only_version_1_1_is_read() {
    let mut bytes = write(&sample_root());
    bytes[0] = 1;
    assert!(read(&bytes).is_err());
    assert!(read(&bytes[..10]).is_err());
}

#[test]
fn a_captured_container_yields_its_payload() {
    let rts0 = write(&sample_root());
    let captured = container(&[(b"RTS0", &rts0)]);
    assert_eq!(payload(&captured).unwrap(), rts0.as_slice());
    assert_eq!(payload(&rts0).unwrap(), rts0.as_slice());
}

#[test]
fn with_root_appends_an_rts0_part_and_keeps_the_others() {
    let shader = container(&[(b"DXIL", &[1, 2, 3, 4]), (b"PSV0", &[5, 6, 7, 8, 9, 10, 11, 12])]);
    let rts0 = write(&sample_root());
    let request = with_root(&shader, &rts0).unwrap();
    let dxbc = Dxbc::parse(&request).unwrap();
    let part = |tag| dxbc.chunk(tag).map(|c| &request[c.offset..c.offset + c.size]).unwrap();
    assert_eq!(part(b"DXIL"), &[1, 2, 3, 4]);
    assert_eq!(part(b"PSV0"), &[5, 6, 7, 8, 9, 10, 11, 12]);
    assert_eq!(part(b"RTS0"), rts0.as_slice());
    assert!(with_root(&request, &rts0).is_err(), "an embedded root is replaced, not appended");
}

#[test]
fn describe_groups_runs_of_one_type() {
    let text = describe(&sample_root());
    assert!(text.contains("slots 0-1: SRV, space0: t78,t311"), "{text}");
    assert!(text.contains("slots 2-2: UAV, space0: u7"), "{text}");
    assert!(text.contains("root SRV t1, space3"), "{text}");
    assert!(text.contains("3 root constants b502, space0"), "{text}");
}

#[test]
fn uncovered_lists_registers_the_root_lacks() {
    let hlsl = "RWBuffer<uint> a : register(u7);\nBuffer<uint> b : register(t311, space0);\n\
                ByteAddressBuffer c : register(t5, space3);\nSamplerState d : register(s3, space1);\n\
                SamplerState e : register(s0, space32765);\ncbuffer f : register(b502) {}";
    assert_eq!(crate::records::uncovered(hlsl, &sample_root()), vec!["t5,space3"]);
}
