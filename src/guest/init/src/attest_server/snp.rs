//! SNP hardware interaction and attestation report retrieval.

#[cfg(target_os = "linux")]
use std::io::Read;

/// SNP attestation report size (AMD SEV-SNP ABI spec v1.52).
#[cfg(target_os = "linux")]
const SNP_REPORT_SIZE: usize = 1184;

/// Certificate chain from the SNP extended report.
#[cfg(target_os = "linux")]
#[derive(serde::Serialize, Default)]
pub(super) struct CertChain {
    pub(super) vcek: Vec<u8>,
    pub(super) ask: Vec<u8>,
    pub(super) ark: Vec<u8>,
}

/// Attestation response (used internally for SNP report + certs).
#[cfg(target_os = "linux")]
pub(super) struct AttestResponse {
    pub(super) report: Vec<u8>,
    pub(super) cert_chain: CertChain,
}

// ============================================================================
// SNP ioctl interface (/dev/sev-guest)
// ============================================================================

/// Get an SNP attestation report from the hardware via `/dev/sev-guest`.
#[cfg(target_os = "linux")]
pub(super) fn get_snp_report(
    report_data: &[u8; super::SNP_USER_DATA_SIZE],
) -> Result<AttestResponse, String> {
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;

    let dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/sev-guest")
        .or_else(|_| OpenOptions::new().read(true).write(true).open("/dev/sev"))
        .map_err(|e| format!("Cannot open SEV device: {} (is this a SEV-SNP VM?)", e))?;

    let fd = dev.as_raw_fd();

    // First try SNP_GET_EXT_REPORT (report + certs)
    match snp_get_ext_report(fd, report_data) {
        Ok(resp) => return Ok(resp),
        Err(e) => {
            tracing::debug!(
                "SNP_GET_EXT_REPORT failed ({}), falling back to SNP_GET_REPORT",
                e
            );
        }
    }

    // Fallback: SNP_GET_REPORT (report only, no certs)
    let report = snp_get_report(fd, report_data)?;
    Ok(AttestResponse {
        report,
        cert_chain: CertChain::default(),
    })
}

// ============================================================================
// SNP ioctl structures (from linux/sev-guest.h)
// ============================================================================

#[cfg(target_os = "linux")]
const SNP_GET_REPORT_IOCTL: libc::c_ulong = 0xc018_5300;
#[cfg(target_os = "linux")]
const SNP_GET_EXT_REPORT_IOCTL: libc::c_ulong = 0xc018_5302;

#[cfg(target_os = "linux")]
#[repr(C)]
struct SnpReportReq {
    user_data: [u8; 64],
    vmpl: u32,
    rsvd: [u8; 28],
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct SnpReportResp {
    status: u32,
    report_size: u32,
    rsvd: [u8; 24],
    report: [u8; SNP_REPORT_SIZE],
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct SnpGuestRequestIoctl {
    msg_version: u8,
    req_data: u64,
    resp_data: u64,
    fw_err: u64,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct SnpExtReportReq {
    data: SnpReportReq,
    certs_address: u64,
    certs_len: u32,
}

/// Get SNP report via SNP_GET_REPORT ioctl.
#[cfg(target_os = "linux")]
fn snp_get_report(
    fd: libc::c_int,
    report_data: &[u8; super::SNP_USER_DATA_SIZE],
) -> Result<Vec<u8>, String> {
    let mut req = SnpReportReq {
        user_data: [0u8; 64],
        vmpl: 0,
        rsvd: [0u8; 28],
    };
    req.user_data.copy_from_slice(report_data);

    let mut resp = SnpReportResp {
        status: 0,
        report_size: 0,
        rsvd: [0u8; 24],
        report: [0u8; SNP_REPORT_SIZE],
    };

    let mut ioctl_req = SnpGuestRequestIoctl {
        msg_version: 1,
        req_data: &req as *const _ as u64,
        resp_data: &mut resp as *mut _ as u64,
        fw_err: 0,
    };

    let ret = unsafe {
        libc::ioctl(
            fd,
            SNP_GET_REPORT_IOCTL as libc::Ioctl,
            &mut ioctl_req as *mut _,
        )
    };

    if ret != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(format!(
            "SNP_GET_REPORT ioctl failed: {} (fw_err: {:#x})",
            errno, ioctl_req.fw_err
        ));
    }

    if resp.status != 0 {
        return Err(format!("SNP_GET_REPORT firmware error: {:#x}", resp.status));
    }

    Ok(resp.report.to_vec())
}

/// Get SNP extended report (report + certificate chain).
#[cfg(target_os = "linux")]
fn snp_get_ext_report(
    fd: libc::c_int,
    report_data: &[u8; super::SNP_USER_DATA_SIZE],
) -> Result<AttestResponse, String> {
    const CERTS_BUF_SIZE: usize = 16384;
    let mut certs_buf = vec![0u8; CERTS_BUF_SIZE];

    let mut report_req = SnpReportReq {
        user_data: [0u8; 64],
        vmpl: 0,
        rsvd: [0u8; 28],
    };
    report_req.user_data.copy_from_slice(report_data);

    let mut ext_req = SnpExtReportReq {
        data: report_req,
        certs_address: certs_buf.as_mut_ptr() as u64,
        certs_len: CERTS_BUF_SIZE as u32,
    };

    let mut resp = SnpReportResp {
        status: 0,
        report_size: 0,
        rsvd: [0u8; 24],
        report: [0u8; SNP_REPORT_SIZE],
    };

    let mut ioctl_req = SnpGuestRequestIoctl {
        msg_version: 1,
        req_data: &mut ext_req as *mut _ as u64,
        resp_data: &mut resp as *mut _ as u64,
        fw_err: 0,
    };

    let ret = unsafe {
        libc::ioctl(
            fd,
            SNP_GET_EXT_REPORT_IOCTL as libc::Ioctl,
            &mut ioctl_req as *mut _,
        )
    };

    if ret != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(format!(
            "SNP_GET_EXT_REPORT ioctl failed: {} (fw_err: {:#x})",
            errno, ioctl_req.fw_err
        ));
    }

    if resp.status != 0 {
        return Err(format!(
            "SNP_GET_EXT_REPORT firmware error: {:#x}",
            resp.status
        ));
    }

    let cert_chain = parse_cert_table(&certs_buf, ext_req.certs_len as usize);

    Ok(AttestResponse {
        report: resp.report.to_vec(),
        cert_chain,
    })
}

/// Parse the SNP certificate table returned by SNP_GET_EXT_REPORT.
#[cfg(target_os = "linux")]
fn parse_cert_table(buf: &[u8], len: usize) -> CertChain {
    const VCEK_GUID: [u8; 16] = guid_bytes("63da758d-e664-4564-adc5-f4b93be8accd");
    const ASK_GUID: [u8; 16] = guid_bytes("4ab7b379-bbac-4fe4-a02f-05aef327c782");
    const ARK_GUID: [u8; 16] = guid_bytes("c0b406a4-a803-4952-9743-3fb6014cd0ae");

    let mut chain = CertChain::default();
    if len < 24 {
        return chain;
    }

    let mut pos = 0;
    while pos + 24 <= len {
        let guid = &buf[pos..pos + 16];
        if guid.iter().all(|&b| b == 0) {
            break;
        }

        let offset =
            u32::from_le_bytes(buf[pos + 16..pos + 20].try_into().unwrap_or([0; 4])) as usize;
        let cert_len =
            u32::from_le_bytes(buf[pos + 20..pos + 24].try_into().unwrap_or([0; 4])) as usize;

        if offset + cert_len <= len {
            let cert_data = buf[offset..offset + cert_len].to_vec();
            if guid == VCEK_GUID {
                chain.vcek = cert_data;
            } else if guid == ASK_GUID {
                chain.ask = cert_data;
            } else if guid == ARK_GUID {
                chain.ark = cert_data;
            }
        }

        pos += 24;
    }

    chain
}

/// Convert a UUID string to little-endian bytes (AMD SEV-SNP format).
#[cfg(target_os = "linux")]
pub(super) const fn guid_bytes(uuid: &str) -> [u8; 16] {
    let b = uuid.as_bytes();
    let mut out = [0u8; 16];

    let mut hex = [0u8; 32];
    let mut hi = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'-' {
            hex[hi] = hex_val(b[i]);
            hi += 1;
        }
        i += 1;
    }

    out[0] = hex[6] << 4 | hex[7];
    out[1] = hex[4] << 4 | hex[5];
    out[2] = hex[2] << 4 | hex[3];
    out[3] = hex[0] << 4 | hex[1];
    out[4] = hex[10] << 4 | hex[11];
    out[5] = hex[8] << 4 | hex[9];
    out[6] = hex[14] << 4 | hex[15];
    out[7] = hex[12] << 4 | hex[13];
    let mut j = 0;
    while j < 8 {
        out[8 + j] = hex[16 + j * 2] << 4 | hex[16 + j * 2 + 1];
        j += 1;
    }

    out
}

/// Convert a hex ASCII byte to its numeric value (const fn compatible).
#[cfg(target_os = "linux")]
pub(super) const fn hex_val(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}
