// SPDX-FileCopyrightText: Copyright (c) 2002, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2007-2013, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! CADRG scale catalogue from GDAL 3.13.3 frmts/nitf/nitffile.cpp.
//! Entries whose scale is variable require an explicit caller-supplied scale.

pub(super) fn scale(series: &str) -> Option<u32> {
    match series {
        "V1" => Some(50),
        "K1" => Some(8_000),
        "A1" | "C1" | "K2" => Some(10_000),
        "C2" | "K3" => Some(10_560),
        "C3" => Some(11_000),
        "C4" => Some(11_800),
        "C5" => Some(12_000),
        "C6" | "K7" => Some(12_500),
        "C7" | "K8" => Some(12_800),
        "C8" => Some(14_000),
        "C9" => Some(14_700),
        "CA" | "KB" => Some(15_000),
        "CB" => Some(15_500),
        "CC" => Some(16_000),
        "CD" | "KE" => Some(16_666),
        "CE" => Some(17_000),
        "CF" => Some(17_500),
        "CH" => Some(18_000),
        "CJ" => Some(20_000),
        "CK" => Some(21_000),
        "CL" | "KM" => Some(21_120),
        "CN" => Some(22_000),
        "CP" => Some(23_000),
        "TQ" => Some(24_000),
        "A2" | "CQ" | "KR" | "MH" | "P1" | "P2" | "P3" | "P4" | "TT" => Some(25_000),
        "CR" | "KS" => Some(26_000),
        "CS" => Some(35_000),
        "CT" | "KU" => Some(36_000),
        "A3" | "MI" | "P5" | "P6" | "P7" | "P8" | "R1" | "RL" | "RV" | "TL" | "UL" => Some(50_000),
        "V2" => Some(62_500),
        "V3" => Some(90_000),
        "A4" | "MJ" | "P9" | "PA" | "PB" | "PC" | "R2" | "RC" | "TC" => Some(100_000),
        "VH" => Some(125_000),
        "AT" | "RR" | "TR" => Some(200_000),
        "ET" | "F1" | "F2" | "F3" | "F4" | "F5" | "JA" | "JG" | "JO" | "JR" | "PD" | "PE" | "PF" | "PG" | "R3" | "TF" | "TN" | "V4"
        | "VT" | "WA" | "YA" | "ZA" => Some(250_000),
        "ES" | "L1" | "L2" | "L3" | "L4" | "L5" | "LF" | "LN" | "PH" | "PI" | "PJ" | "PK" | "R4" | "TP" | "VN" | "WB" | "YB" | "ZB" => {
            Some(500_000)
        }
        "WC" | "YC" | "ZC" => Some(750_000),
        "OH" | "ON" | "OW" | "PL" | "PM" | "PN" | "PO" | "R5" | "WD" | "XD" | "YD" | "ZD" => Some(1_000_000),
        "WE" | "XE" | "YE" | "ZE" => Some(1_500_000),
        "JN" | "PP" | "PQ" | "PR" | "WF" | "XF" | "YF" | "ZF" => Some(2_000_000),
        "WG" | "XG" | "ZG" => Some(2_500_000),
        "WH" | "XH" | "ZH" => Some(3_000_000),
        "WI" | "XI" | "YI" | "ZI" => Some(3_500_000),
        "WK" | "XJ" | "YJ" | "ZJ" => Some(4_000_000),
        "XK" | "ZK" => Some(4_500_000),
        "GN" | "PS" | "PT" | "PU" | "PV" => Some(5_000_000),
        "ZT" => Some(9_000_000),
        "ZV" => Some(10_000_000),
        "YZ" | "ZZ" => Some(12_000_000),
        "Y9" => Some(16_500_000),
        _ => None,
    }
}
