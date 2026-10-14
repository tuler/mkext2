//! Malformed images must return errors before their geometry is used.

use mkext2::ondisk::*;
use mkext2::reader::Image;
use mkext2::{Options, Source, Timestamps, build_vec};

fn image() -> Vec<u8> {
    build_vec(&Options {
        size_in_blocks: Some(64),
        timestamps: Timestamps::Zero,
        ..Options::default()
    })
    .unwrap()
    .0
}

#[test]
fn reader_rejects_invalid_geometry() {
    for (field, value) in [(4, 0), (20, 0), (32, 8193), (40, 8193)] {
        let mut data = image();
        wr32(&mut data, SUPERBLOCK_OFFSET as usize + field, value);
        assert!(Image::new(data).is_err(), "field {field}: {value}");
    }
}

#[test]
fn reader_rejects_truncated_descriptor_table() {
    let mut data = image();
    data.truncate(2048 + GROUP_DESC_SIZE - 1);
    assert!(Image::new(data).is_err());
}

#[test]
fn starting_image_rejects_inconsistent_inode_count() {
    let mut data = image();
    wr32(&mut data, SUPERBLOCK_OFFSET as usize, u32::MAX);
    assert!(
        build_vec(&Options {
            starting_image: Some(Source::Bytes(data.into())),
            timestamps: Timestamps::Zero,
            ..Options::default()
        })
        .is_err()
    );
}

#[test]
fn starting_image_rejects_invalid_geometry() {
    for (field, value) in [(4, 0), (20, 0), (32, 0), (40, 0), (32, 8200)] {
        let mut data = image();
        wr32(&mut data, SUPERBLOCK_OFFSET as usize + field, value);
        assert!(
            build_vec(&Options {
                starting_image: Some(Source::Bytes(data.into())),
                timestamps: Timestamps::Zero,
                ..Options::default()
            })
            .is_err(),
            "field {field}: {value}"
        );
    }
}
