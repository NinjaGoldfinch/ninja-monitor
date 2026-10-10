//! Turns stored points into compact columnar responses, averaging fixed-size
//! buckets when a window holds more points than the caller asked for.

use serde::Serialize;

use crate::sampler::round2;

use crate::store::{
    DiskPoint, HostPoint, ServicePoint, Timestamped, unpack_f32, unpack_u32, unpack_u64,
};

#[derive(Debug, Serialize)]
pub struct HostHistory {
    /// Raw samples averaged into each point.
    pub bucket: usize,
    pub t: Vec<u64>,
    pub cpu_percent: Vec<Option<f64>>,
    pub load1: Vec<Option<f64>>,
    pub load5: Vec<Option<f64>>,
    pub load15: Vec<Option<f64>>,
    pub mem_available_bytes: Vec<Option<u64>>,
    pub mem_used_bytes: Vec<Option<u64>>,
    pub mem_cache_bytes: Vec<Option<u64>>,
    pub swap_used_bytes: Vec<Option<u64>>,
    pub disks: Vec<DiskHistory>,
}

#[derive(Debug, Serialize)]
pub struct DiskHistory {
    pub mount: String,
    pub used_bytes: Vec<Option<u64>>,
    pub used_percent: Vec<Option<f64>>,
}

#[derive(Debug, Serialize)]
pub struct ServiceHistory {
    pub bucket: usize,
    pub t: Vec<u64>,
    /// Fraction of samples in the bucket where the service was running (0 to 1).
    pub up: Vec<f64>,
    pub cpu_percent: Vec<Option<f64>>,
    pub pids: Vec<Option<u64>>,
    pub mem_current_bytes: Vec<Option<u64>>,
    pub mem_anon_bytes: Vec<Option<u64>>,
    pub mem_file_bytes: Vec<Option<u64>>,
    /// Highest counter value in the bucket.
    pub oom_kills: Vec<Option<u64>>,
    pub disk_bytes: Vec<Option<u64>>,
}

/// Samples per output point so that at most `max_points` come back.
pub fn bucket_size(len: usize, max_points: usize) -> usize {
    let max_points = max_points.max(1);
    if len <= max_points {
        1
    } else {
        len.div_ceil(max_points)
    }
}

pub fn host(
    points: &[HostPoint],
    disks: &[(String, Vec<DiskPoint>)],
    max_points: usize,
) -> HostHistory {
    let size = bucket_size(points.len(), max_points);
    HostHistory {
        bucket: size,
        t: times(points, size),
        cpu_percent: mean_f32(points, size, |p| p.cpu_percent),
        load1: mean_f32(points, size, |p| p.load1),
        load5: mean_f32(points, size, |p| p.load5),
        load15: mean_f32(points, size, |p| p.load15),
        mem_available_bytes: mean_u64(points, size, |p| unpack_u64(p.mem_available_bytes)),
        mem_used_bytes: mean_u64(points, size, |p| unpack_u64(p.mem_used_bytes)),
        mem_cache_bytes: mean_u64(points, size, |p| unpack_u64(p.mem_cache_bytes)),
        swap_used_bytes: mean_u64(points, size, |p| unpack_u64(p.swap_used_bytes)),
        disks: disks
            .iter()
            .map(|(mount, points)| DiskHistory {
                mount: mount.clone(),
                used_bytes: mean_u64(points, size, |p| unpack_u64(p.used_bytes)),
                used_percent: mean_f32(points, size, |p| p.used_percent),
            })
            .collect(),
    }
}

pub fn service(points: &[ServicePoint], max_points: usize) -> ServiceHistory {
    let size = bucket_size(points.len(), max_points);
    ServiceHistory {
        bucket: size,
        t: times(points, size),
        up: points
            .chunks(size)
            .map(|c| round2(c.iter().filter(|p| p.up).count() as f64 / c.len() as f64))
            .collect(),
        cpu_percent: mean_f32(points, size, |p| p.cpu_percent),
        pids: mean_u64(points, size, |p| unpack_u32(p.pids)),
        mem_current_bytes: mean_u64(points, size, |p| unpack_u64(p.mem_current_bytes)),
        mem_anon_bytes: mean_u64(points, size, |p| unpack_u64(p.mem_anon_bytes)),
        mem_file_bytes: mean_u64(points, size, |p| unpack_u64(p.mem_file_bytes)),
        oom_kills: points
            .chunks(size)
            .map(|c| c.iter().filter_map(|p| unpack_u64(p.oom_kills)).max())
            .collect(),
        disk_bytes: mean_u64(points, size, |p| unpack_u64(p.disk_bytes)),
    }
}

/// Each bucket is labelled with the time of its first sample.
fn times<P: Timestamped>(points: &[P], size: usize) -> Vec<u64> {
    points.chunks(size).map(|c| c[0].t()).collect()
}

/// Mean of the values present in each bucket; `None` if all are missing.
fn mean_f32<P>(points: &[P], size: usize, get: impl Fn(&P) -> f32) -> Vec<Option<f64>> {
    points
        .chunks(size)
        .map(|c| mean(c.iter().filter_map(|p| unpack_f32(get(p)))).map(round2))
        .collect()
}

fn mean_u64<P>(points: &[P], size: usize, get: impl Fn(&P) -> Option<u64>) -> Vec<Option<u64>> {
    points
        .chunks(size)
        .map(|c| mean(c.iter().filter_map(&get).map(|v| v as f64)).map(|m| m.round() as u64))
        .collect()
}

fn mean(values: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, n) = values.fold((0.0, 0u32), |(sum, n), v| (sum + v, n + 1));
    (n > 0).then(|| sum / n as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{pack_f32, pack_u32, pack_u64};

    fn svc(t: u64, up: bool, cpu: Option<f64>, anon: Option<u64>, oom: u64) -> ServicePoint {
        ServicePoint {
            t,
            up,
            cpu_percent: pack_f32(cpu),
            pids: pack_u32(Some(4)),
            mem_current_bytes: pack_u64(None),
            mem_anon_bytes: pack_u64(anon),
            mem_file_bytes: pack_u64(Some(10)),
            oom_kills: pack_u64(Some(oom)),
            disk_bytes: pack_u64(None),
        }
    }

    #[test]
    fn bucket_sizes() {
        assert_eq!(bucket_size(0, 720), 1);
        assert_eq!(bucket_size(720, 720), 1);
        assert_eq!(bucket_size(721, 720), 2);
        assert_eq!(bucket_size(5760, 720), 8);
        assert_eq!(bucket_size(10, 3), 4);
        assert_eq!(bucket_size(10, 0), 10);
    }

    #[test]
    fn no_downsampling_when_small() {
        let points: Vec<_> = (0..3)
            .map(|i| svc(i, true, Some(i as f64), Some(i), 0))
            .collect();
        let h = service(&points, 720);
        assert_eq!(h.bucket, 1);
        assert_eq!(h.t, [0, 1, 2]);
        assert_eq!(h.cpu_percent, [Some(0.0), Some(1.0), Some(2.0)]);
        assert_eq!(h.mem_anon_bytes, [Some(0), Some(1), Some(2)]);
        assert_eq!(h.up, [1.0, 1.0, 1.0]);
    }

    #[test]
    fn averages_fixed_buckets() {
        // 10 points into at most 3: buckets of 4, 4 and 2.
        let points: Vec<_> = (0..10)
            .map(|i| svc(i * 10, i % 2 == 0, Some(i as f64), Some(i * 100), i / 3))
            .collect();
        let h = service(&points, 3);
        assert_eq!(h.bucket, 4);
        assert_eq!(h.t, [0, 40, 80]);
        assert_eq!(h.cpu_percent, [Some(1.5), Some(5.5), Some(8.5)]);
        assert_eq!(h.mem_anon_bytes, [Some(150), Some(550), Some(850)]);
        assert_eq!(h.up, [0.5, 0.5, 0.5]);
        assert_eq!(h.oom_kills, [Some(1), Some(2), Some(3)]);
        assert_eq!(h.pids, [Some(4), Some(4), Some(4)]);
        assert_eq!(h.disk_bytes, [None, None, None]);
        let lens = [
            h.t.len(),
            h.up.len(),
            h.cpu_percent.len(),
            h.mem_file_bytes.len(),
        ];
        assert!(lens.iter().all(|&l| l == 3));
    }

    #[test]
    fn missing_values_are_skipped_not_zeroed() {
        let points = [
            svc(0, true, None, None, 0),
            svc(1, true, Some(30.0), Some(100), 0),
            svc(2, false, None, None, 0),
            svc(3, false, None, None, 0),
        ];
        let h = service(&points, 2);
        assert_eq!(h.cpu_percent, [Some(30.0), None]);
        assert_eq!(h.mem_anon_bytes, [Some(100), None]);
        assert_eq!(h.up, [1.0, 0.0]);
    }

    #[test]
    fn host_with_disks() {
        let points: Vec<_> = (0..4)
            .map(|i| HostPoint {
                t: i,
                cpu_percent: pack_f32(Some(10.0 * i as f64)),
                load1: pack_f32(Some(0.33)),
                load5: pack_f32(None),
                load15: pack_f32(None),
                mem_available_bytes: pack_u64(Some(1000 + i)),
                mem_used_bytes: pack_u64(Some(1)),
                mem_cache_bytes: pack_u64(None),
                swap_used_bytes: pack_u64(Some(0)),
            })
            .collect();
        let disks = vec![(
            "/".to_string(),
            (0..4)
                .map(|i| DiskPoint {
                    t: i,
                    used_bytes: pack_u64(Some(i)),
                    used_percent: pack_f32(Some(50.0)),
                })
                .collect(),
        )];
        let h = host(&points, &disks, 2);
        assert_eq!(h.bucket, 2);
        assert_eq!(h.t, [0, 2]);
        assert_eq!(h.cpu_percent, [Some(5.0), Some(25.0)]);
        // f32 storage must not leak noise like 0.33000001311.
        assert_eq!(h.load1, [Some(0.33), Some(0.33)]);
        assert_eq!(h.load5, [None, None]);
        assert_eq!(h.mem_available_bytes, [Some(1001), Some(1003)]);
        assert_eq!(h.disks[0].mount, "/");
        assert_eq!(h.disks[0].used_bytes, [Some(1), Some(3)]);
        assert_eq!(h.disks[0].used_percent, [Some(50.0), Some(50.0)]);
    }

    #[test]
    fn empty_window() {
        let h = service(&[], 720);
        assert!(h.t.is_empty() && h.up.is_empty());
    }
}
