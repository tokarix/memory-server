//! Mount parsing and injected sysfs topology fixtures.

use std::fmt::Write;

use super::*;

#[derive(Default)]
struct TopologyFixture {
    files: BTreeMap<PathBuf, Vec<u8>>,
    directories: BTreeMap<PathBuf, Vec<OsString>>,
    links: BTreeMap<PathBuf, PathBuf>,
}

impl TopologyFixture {
    fn node(&mut self, major: u32, minor: u32, path: &str) -> PathBuf {
        let path = PathBuf::from(path);
        self.links.insert(
            PathBuf::from(format!("/sys/dev/block/{major}:{minor}")),
            path.clone(),
        );
        self.files
            .insert(path.join("dev"), format!("{major}:{minor}\n").into_bytes());
        self.files.insert(path.join("size"), b"10000\n".to_vec());
        self.directories.insert(path.join("slaves"), Vec::new());
        if !path.starts_with("/sys/devices/virtual") {
            self.links.insert(
                path.join("driver"),
                PathBuf::from("/sys/bus/pci/drivers/virtio_scsi"),
            );
        }
        path
    }
    fn slave(&mut self, path: &Path, name: &str, major: u32, minor: u32) {
        self.directories
            .get_mut(&path.join("slaves"))
            .unwrap()
            .push(OsString::from(name));
        self.files.insert(
            path.join("slaves").join(name).join("dev"),
            format!("{major}:{minor}\n").into_bytes(),
        );
    }
    fn classify(&mut self, device: (u32, u32)) -> ProbeResult<BackingClass> {
        Backing {
            provider: self,
            nodes: 0,
        }
        .device(device, 0, &mut BTreeSet::new(), &mut Budget::new())
    }
}

impl Topology for TopologyFixture {
    fn read(&mut self, path: &Path, budget: &mut Budget) -> ProbeResult<Vec<u8>> {
        budget.tick()?;
        self.files.get(path).cloned().ok_or(ProbeError {
            reason: Reason::Inaccessible,
            code: Some(libc::ENOENT),
        })
    }
    fn directory(&mut self, path: &Path, budget: &mut Budget) -> ProbeResult<Vec<OsString>> {
        budget.tick()?;
        self.directories.get(path).cloned().ok_or(ProbeError {
            reason: Reason::Inaccessible,
            code: Some(libc::ENOENT),
        })
    }
    fn canonical(&mut self, path: &Path, budget: &mut Budget) -> ProbeResult<PathBuf> {
        budget.tick()?;
        self.links.get(path).cloned().ok_or(ProbeError {
            reason: Reason::Inaccessible,
            code: Some(libc::ENOENT),
        })
    }
}

#[test]
fn escaped_fields_optional_rows_mount_roots_and_fdinfo() {
    let table = b"10 9 8:1 /sub\\040volume /mnt\\134name\\011tab\\012line rw shared:4 future:unknown - ext4 secret rw,secret-option\n11 999 8:1 /other /mnt rw - ext4 disk rw\n12 11 0:3 / /mnt/nested rw - tmpfs none rw\n13 11 8:1 / /mnt rw - ext4 same-device rw\n";
    let mounts = mountinfo(table).unwrap();
    assert_eq!(mounts.len(), 4);
    assert_eq!(mounts[&10].root, Path::new("/sub volume"));
    assert_eq!(mounts[&10].mountpoint, Path::new("/mnt\\name\ttab\nline"));
    assert_ne!(mounts[&11].row_hash, mounts[&13].row_hash);
    assert_eq!(
        fdinfo_mount_id(b"pos:\t0\nflags:\t010000000\nmnt_id:\t13\nino:\t42\n").unwrap(),
        13
    );
    assert!(fdinfo_mount_id(b"mnt_id: 13\nmnt_id: 13\n").is_err());
    assert!(fdinfo_mount_id(b"mnt_id: nope\n").is_err());
}

#[test]
fn malformed_truncated_duplicate_and_excessive_mount_tables() {
    for table in [
        b"".as_slice(),
        b"1 9 8:1 / / rw - ext4 disk rw",
        b"1 9 8:x / / rw - ext4 disk rw\n",
        b"1 9 8:1 /bad\\999 / rw - ext4 disk rw\n",
        b"1 9 8:1 / / rw ext4 disk rw\n",
        b"1 1 8:1 / / rw - ext4 disk rw\n",
        b"1 9 8:1 / / rw - ext4 disk rw\n1 9 0:2 / / rw - tmpfs none rw\n",
    ] {
        assert!(mountinfo(table).is_err());
    }
    let mut rows = String::new();
    for id in 1..=MOUNT_ROWS {
        writeln!(rows, "{id} 99999 8:1 / / rw - ext4 disk rw").unwrap();
    }
    assert!(mountinfo(rows.as_bytes()).is_ok());
    assert!(
        mountinfo(format!("{rows}99998 99999 8:1 / / rw - ext4 disk rw\n").as_bytes()).is_err()
    );
    assert!(mountinfo(&vec![b'x'; MOUNT_BYTES + 1]).is_err());
    assert!(
        mountinfo(format!("1 9 8:1 / / rw {} - ext4 disk rw\n", "x".repeat(ROW_BYTES)).as_bytes())
            .is_err()
    );
}

#[test]
fn filesystem_magic_never_substitutes_for_device_evidence() {
    for (name, magic, expected) in [
        (
            b"ext4".as_slice(),
            libc::EXT4_SUPER_MAGIC,
            FilesystemClass::Ext,
        ),
        (b"xfs", libc::XFS_SUPER_MAGIC, FilesystemClass::Xfs),
        (b"btrfs", libc::BTRFS_SUPER_MAGIC, FilesystemClass::Btrfs),
        (b"tmpfs", libc::TMPFS_MAGIC, FilesystemClass::Memory),
        (b"ramfs", RAMFS_MAGIC, FilesystemClass::Memory),
        (
            b"overlay",
            libc::OVERLAYFS_SUPER_MAGIC,
            FilesystemClass::Overlay,
        ),
    ] {
        assert_eq!(coherent_filesystem(name, magic).unwrap(), expected);
        assert!(coherent_filesystem(name, libc::PROC_SUPER_MAGIC).is_err());
    }
    for name in [b"nfs".as_slice(), b"fuse", b"virtiofs", b"unknown"] {
        assert_eq!(
            coherent_filesystem(name, 0x1234).unwrap(),
            FilesystemClass::Other
        );
        assert!(coherent_filesystem(name, libc::TMPFS_MAGIC).is_err());
    }
}

#[test]
fn descriptor_mount_identity_selects_exact_bind_and_nested_mounts() {
    let snapshot = Snapshot::fixture(
        b"1 99 8:1 / / rw - ext4 disk rw\n2 1 0:2 / /tmp rw - tmpfs memory rw\n3 2 8:1 /volume /tmp/out rw - ext4 disk rw\n4 1 8:1 /other /tmp/out rw - ext4 disk rw\n",
    );
    for (id, device, magic, path, expected) in [
        (1, (8, 1), libc::EXT4_SUPER_MAGIC, "/", FilesystemClass::Ext),
        (
            2,
            (0, 2),
            libc::TMPFS_MAGIC,
            "/tmp/innocent",
            FilesystemClass::Memory,
        ),
        (
            3,
            (8, 1),
            libc::EXT4_SUPER_MAGIC,
            "/tmp/out/new",
            FilesystemClass::Ext,
        ),
        (
            4,
            (8, 1),
            libc::EXT4_SUPER_MAGIC,
            "/tmp/out/new",
            FilesystemClass::Ext,
        ),
    ] {
        let (mount, class) =
            correlate_mount(&snapshot, id, device, magic, Path::new(path)).unwrap();
        assert_eq!(mount.id, id);
        assert_eq!(class, expected);
    }
    // Same devices and stacked mountpoints cannot select a convenient row.
    for (id, device, magic, path) in [
        (99, (8, 1), libc::EXT4_SUPER_MAGIC, "/tmp/out"),
        (2, (8, 1), libc::TMPFS_MAGIC, "/tmp/out"),
        (3, (0, 2), libc::EXT4_SUPER_MAGIC, "/tmp/out"),
        (3, (8, 1), libc::TMPFS_MAGIC, "/tmp/out"),
        (3, (8, 1), libc::EXT4_SUPER_MAGIC, "/tmp/output"),
    ] {
        assert_eq!(
            correlate_mount(&snapshot, id, device, magic, Path::new(path))
                .err()
                .unwrap()
                .reason,
            Reason::MountEvidence
        );
    }
}

#[test]
fn btrfs_inventory_requires_complete_distinct_filesystem_devices() {
    let id = uuid::Uuid::from_u128(42);
    let directory = PathBuf::from(format!("/sys/fs/btrfs/{id}/devices"));
    let mut fixture = TopologyFixture::default();
    fixture.node(8, 0, "/sys/devices/pci/scsi/block/sda");
    fixture.node(8, 16, "/sys/devices/pci/scsi/block/sdb");
    fixture
        .directories
        .insert(directory.clone(), vec!["second".into(), "first".into()]);
    fixture
        .files
        .insert(directory.join("first/dev"), b"8:0\n".to_vec());
    fixture
        .files
        .insert(directory.join("second/dev"), b"8:16\n".to_vec());
    let devices = btrfs_inventory(&mut fixture, id, 2, &mut Budget::new()).unwrap();
    assert_eq!(devices, [(8, 0), (8, 16)]);
    let classes = devices
        .iter()
        .map(|device| fixture.classify(*device).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(combine_backing(&classes), BackingClass::Disk);
    fixture.node(1, 0, "/sys/devices/virtual/block/ram0");
    fixture
        .files
        .insert(directory.join("second/dev"), b"1:0\n".to_vec());
    let devices = btrfs_inventory(&mut fixture, id, 2, &mut Budget::new()).unwrap();
    let classes = devices
        .iter()
        .map(|device| fixture.classify(*device).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(combine_backing(&classes), BackingClass::Memory);
    fixture
        .files
        .insert(directory.join("second/dev"), b"100:0\n".to_vec());
    let devices = btrfs_inventory(&mut fixture, id, 2, &mut Budget::new()).unwrap();
    assert!(
        devices
            .iter()
            .any(|device| fixture.classify(*device).is_err())
    );
    fixture
        .files
        .insert(directory.join("second/dev"), b"8:0\n".to_vec());
    assert_eq!(
        btrfs_inventory(&mut fixture, id, 2, &mut Budget::new())
            .unwrap_err()
            .reason,
        Reason::UnknownBacking
    );
    for (filesystem_id, count) in [(id, 0), (id, 1), (id, 3), (id, 129), (uuid::Uuid::nil(), 2)] {
        assert_eq!(
            btrfs_inventory(&mut fixture, filesystem_id, count, &mut Budget::new())
                .unwrap_err()
                .reason,
            Reason::UnknownBacking
        );
    }
    assert!(
        btrfs_inventory(
            &mut fixture,
            uuid::Uuid::from_u128(43),
            2,
            &mut Budget::new()
        )
        .is_err()
    );
    fixture
        .files
        .insert(directory.join("second/dev"), b"malformed\n".to_vec());
    assert_eq!(
        btrfs_inventory(&mut fixture, id, 2, &mut Budget::new())
            .unwrap_err()
            .reason,
        Reason::MountEvidence
    );
    fixture.files.remove(&directory.join("second/dev"));
    assert!(btrfs_inventory(&mut fixture, id, 2, &mut Budget::new()).is_err());
    fixture.directories.insert(directory.clone(), Vec::new());
    assert!(btrfs_inventory(&mut fixture, id, 2, &mut Budget::new()).is_err());
    // The exact inventory bound is accepted before backing traversal; the
    // evaluation-wide backing-node budget still applies to its classification.
    fixture.directories.insert(
        directory.clone(),
        (0..128).map(|n| OsString::from(n.to_string())).collect(),
    );
    for minor in 0..128 {
        fixture.files.insert(
            directory.join(minor.to_string()).join("dev"),
            format!("8:{minor}\n").into_bytes(),
        );
    }
    assert_eq!(
        btrfs_inventory(&mut fixture, id, 128, &mut Budget::new())
            .unwrap()
            .len(),
        128
    );
}

#[test]
fn disk_partition_composite_volatile_loop_and_hidden_topology() {
    let mut fixture = TopologyFixture::default();
    let disk = fixture.node(8, 0, "/sys/devices/pci0000/scsi/block/sda");
    let partition = fixture.node(8, 1, "/sys/devices/pci0000/scsi/block/sda/sda1");
    fixture
        .files
        .insert(partition.join("partition"), b"1\n".to_vec());
    assert_eq!(fixture.classify((8, 0)).unwrap(), BackingClass::Disk);
    assert_eq!(fixture.classify((8, 1)).unwrap(), BackingClass::Disk);
    fixture.links.insert(
        disk.join("driver"),
        PathBuf::from("/sys/bus/pseudo/drivers/scsi_debug"),
    );
    assert_eq!(fixture.classify((8, 0)).unwrap(), BackingClass::Memory);
    fixture.links.insert(
        disk.join("driver"),
        PathBuf::from("/sys/bus/scsi/drivers/iscsi_tcp"),
    );
    assert_eq!(fixture.classify((8, 0)).unwrap(), BackingClass::Unknown);
    fixture.links.insert(
        disk.join("driver"),
        PathBuf::from("/sys/bus/virtio/drivers/virtio_scsi"),
    );
    for name in ["ram0", "zram0", "loop0", "mystery0"] {
        fixture.node(9, 0, &format!("/sys/devices/virtual/block/{name}"));
        assert_eq!(
            fixture.classify((9, 0)).unwrap(),
            if name == "ram0" || name == "zram0" {
                BackingClass::Memory
            } else {
                BackingClass::Unknown
            }
        );
    }
    let dm = fixture.node(253, 0, "/sys/devices/virtual/block/dm-0");
    fixture.slave(&dm, "sda", 8, 0);
    assert_eq!(fixture.classify((253, 0)).unwrap(), BackingClass::Disk);
    let md = fixture.node(9, 1, "/sys/devices/virtual/block/md1");
    fixture.slave(&md, "dm-0", 253, 0);
    assert_eq!(fixture.classify((9, 1)).unwrap(), BackingClass::Disk);
    fixture.node(1, 0, "/sys/devices/virtual/block/ram0");
    fixture.slave(&dm, "ram0", 1, 0);
    assert_eq!(fixture.classify((9, 1)).unwrap(), BackingClass::Memory);
    fixture.files.insert(disk.join("dev"), b"8:999\n".to_vec());
    assert_eq!(
        fixture.classify((8, 0)).unwrap_err().reason,
        Reason::MountEvidence
    );
    assert!(fixture.classify((100, 0)).is_err());
}

#[test]
fn cycles_and_exact_backing_limits_are_bounded() {
    let mut fixture = TopologyFixture::default();
    for index in 0..=BACKING_DEPTH + 1 {
        let minor = u32::try_from(index).unwrap();
        let path = fixture.node(
            253,
            minor,
            &format!("/sys/devices/virtual/block/dm-{index}"),
        );
        if index > 0 {
            fixture.slave(&path, "prior", 253, minor - 1);
        }
    }
    let disk = fixture.node(8, 0, "/sys/devices/pci/scsi/block/sda");
    let first = Path::new("/sys/devices/virtual/block/dm-0");
    fixture.slave(first, "disk", 8, 0);
    assert_eq!(
        fixture
            .classify((253, u32::try_from(BACKING_DEPTH - 1).unwrap()))
            .unwrap(),
        BackingClass::Disk
    );
    assert_eq!(
        fixture
            .classify((253, u32::try_from(BACKING_DEPTH).unwrap()))
            .unwrap_err()
            .reason,
        Reason::Limit
    );
    fixture
        .directories
        .get_mut(&first.join("slaves"))
        .unwrap()
        .clear();
    fixture.slave(first, "cycle", 253, 1);
    assert_eq!(fixture.classify((253, 0)).unwrap(), BackingClass::Unknown);
    let mut walker = Backing {
        provider: &mut fixture,
        nodes: BACKING_NODES - 1,
    };
    assert_eq!(
        walker
            .device((8, 0), 0, &mut BTreeSet::new(), &mut Budget::new())
            .unwrap(),
        BackingClass::Disk
    );
    assert_eq!(
        walker
            .device((8, 0), 0, &mut BTreeSet::new(), &mut Budget::new())
            .unwrap_err()
            .reason,
        Reason::Limit
    );
    assert!(disk_device(b"vda"));
    assert!(disk_device(b"nvme0n1"));
    assert!(!disk_device(b"sda1"));
    assert!(!disk_device(b"zram0"));
    assert!(disk.ends_with("sda"));
}

fn padded_row(id: usize, bytes: usize) -> Vec<u8> {
    let mut row = format!("{id:08} 99999999 8:1 / / rw ").into_bytes();
    let ending = b" - ext4 source rw";
    while row.len() + ending.len() + 4097 <= bytes {
        row.extend(vec![b'x'; 4096]);
        row.push(b' ');
    }
    row.extend(vec![b'x'; bytes - row.len() - ending.len()]);
    row.extend(ending);
    row.push(b'\n');
    row
}

#[test]
fn exact_mount_byte_row_and_field_limits() {
    assert!(mountinfo(&padded_row(1, ROW_BYTES)).is_ok());
    assert!(mountinfo(&padded_row(1, ROW_BYTES + 1)).is_err());
    let mut bytes = Vec::new();
    for id in 1..=64 {
        bytes.extend(padded_row(id, ROW_BYTES - 1));
    }
    assert_eq!(bytes.len(), MOUNT_BYTES);
    assert!(mountinfo(&bytes).is_ok());
    bytes.push(b'\n');
    assert!(mountinfo(&bytes).is_err());
    let optional = vec!["future:x"; 70].join(" ");
    assert!(mountinfo(format!("1 99 8:1 / / rw {optional} - ext4 disk rw\n").as_bytes()).is_ok());
    assert!(
        mountinfo(format!("1 99 8:1 / / rw {optional} extra - ext4 disk rw\n").as_bytes()).is_err()
    );
}

#[test]
fn nonstandard_filesystem_credentials_cannot_borrow_effective_access() {
    assert!(check_fs_credentials(b"Uid:\t1\t2\t3\t2\nGid:\t1\t2\t3\t2\n").is_ok());
    for status in [
        b"Uid: 1 2 3 4\nGid: 1 2 3 2\n".as_slice(),
        b"Uid: 1 2 3 2\nGid: 1 2 3 4\n",
        b"Uid: 1 2\nGid: 1 2 3 2\n",
        b"Gid: 1 2 3 2\n",
    ] {
        assert!(check_fs_credentials(status).is_err());
    }
}
