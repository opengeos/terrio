# SPDX-FileCopyrightText: Copyright (c) 1998, 2002, Frank Warmerdam <warmerdam@pobox.com>
# SPDX-FileCopyrightText: Copyright (c) 2007-2015, Even Rouault <even dot rouault at spatialys dot com>
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
#
# Derived from (translated to Python and modified):
#   GDAL v3.12.1:
#     frmts/gtiff/gtiffdataset_write.cpp
#     frmts/gtiff/quant_table_md5sum.h
#     frmts/gtiff/quant_table_md5sum_jpeg9e.h

"""The JPEG quality of a JPEG-compressed TIFF, guessed as GDAL guesses it.

GDAL's GTiff driver reports `JPEG_QUALITY` and `JPEGTABLESMODE` in a JPEG-compressed
TIFF's `IMAGE_STRUCTURE` metadata: the quality whose quantization tables have the MD5 sum of
the file's `JPEGTables` (`GTiffDataset::GuessJPEGQuality`, GDAL 3.12.1). The sums below are
GDAL's (`frmts/gtiff/quant_table_md5sum.h` and `quant_table_md5sum_jpeg9e.h`), quality 1
first. GDAL finds the quality of other layouts by writing temporary files at every quality and
comparing their tables: for 8-bit samples other than YCbCr (two bands, RGBA, three gray bands)
libtiff writes the luminance table alone, as for one band, so the generic sums give it. terrio
reports none for 12-bit samples, nor for YCbCr of other than three bands, which GDAL cannot write.
"""

from __future__ import annotations

import hashlib

_QUANT_TABLE = 0xDB
_HUFFMAN_TABLE = 0xC4

# 1 band (MINISBLACK), 3 bands RGB or 4 bands CMYK, 8 bits.
_GENERIC_8BIT = (
    "f930bb47fbde76e39674631183055eb6",
    "db392ef56b4cb61fec0b8d41dd654cf6",
    "6a4dfa4fd424295f5add625fd8a7a5e1",
    "5c0060c7984059c575dc10c968ee88e6",
    "5bfe2d8749df483005750fa078af4956",
    "f12a423d3c6b8a0132cfb837039a5dee",
    "f4206548578f7bf1d5dd778392042e1c",
    "08618554810c1fbb2e5f65b8de7bae96",
    "a20752f9fb7771090e30bd42642d0afe",
    "2fa43e7b25484fc1deaa8269ec614822",
    "34074313cd2502548f5e895cf21eeaa1",
    "48aae191ae65aa144381f559ab59a8e7",
    "f1b4205ae59af1720722941bfe07e145",
    "c31807aac1dc780fff7b0287c705aae6",
    "c5afb9bfd065aa6235fba2b529efc6e7",
    "93add44a3c75633b9de9c736c60622a1",
    "4c9865dd5635f19d1a8b326e089e6333",
    "f9d472d498f9200654e563e124ca4af4",
    "9dbac7033abe5768029816793c2ad7aa",
    "8a810cc0b5b886a14bd3e6e305cc78fa",
    "6a620af9bc5e23167b198669246973f3",
    "80eca48ecb7e61bf13fb3e878fc2f3ef",
    "8359073a8222939f83f5478f85a597e0",
    "f250a120fe7cd4d54f15a851778c6ece",
    "0f74952dbc1e04fe00aaeaf5797d9eb6",
    "b88f753c001a0a8cc0c38072240f5621",
    "2d2ce8bb5dd6e90b06a54a1fab9b45be",
    "cede691a46c909ba737e33e0031de1ec",
    "f118c787084dd1f48828280b61c43a64",
    "d02c540d988781a08501743763f028c9",
    "a3b716dc2aa847807c19e7296478f7a6",
    "3b3bd1db6c897f2d62969b97e598798b",
    "267a8f1d16406ec77b817f6a210a0f28",
    "c1479ced7a0730e71c7eb7fefa4e87c3",
    "d32dd9b7c429e0c5bbff5ac20e8d7db6",
    "d0b531e1d4a5856c217fb756edf00065",
    "f3b3babe51b60f95e483682d050c516f",
    "be48316cde3d67a043c966ff01484924",
    "c11ca27e5fa00a5080a8a70dfc8864b4",
    "2847ccd81766e28ea228bba44559b917",
    "2679b9d4d57b6ec1d7ca64519a4b418d",
    "44f043003c6a08054d68b83f6fd14724",
    "7b3b1fc840bccc3888a2d3e663f2970f",
    "fdeb7bc9925399ffacbec9bba626b2a2",
    "bf39813b2a05f26984dcb42eac209fd7",
    "9df12a97ecad0de72b47f815e7cf1f90",
    "6acf79783149a4b79ef29c05b3d8b714",
    "bc4e466f2f47200f01ea1761560b31c3",
    "c5a0fc0bc06bb28218c438acca556d20",
    "97a674ee97943d04d4abe0707f4d2a70",
    "50568b434c8b8684a8cce5ebdb7a11f8",
    "6386d78f7cec0ed8c7d0da9440c15e0a",
    "4f67692eab1070ff059841212085e848",
    "adb6c939884eb0528fbf942d78a1c8fb",
    "d1e7fe2e327c3beb49ccd1f70e35f6ec",
    "cd00e3c73c954e1105dc0857da2b7dd8",
    "56fecf89c0b6bf384e57918f1aae40c3",
    "93c019aed9baae7443ae692dcc2c7d53",
    "40bee0e8b14de55b3c5b24fadd8721eb",
    "889a3a6edd4e63dfb82251950dc7fa3e",
    "356ae946498a3010c1a15a8ed0ed7028",
    "b84e01f496f407ce939bf6681f2eeab8",
    "3cf017ed2a68c6fd20f2cd4992c6b1f8",
    "fac1419993727b3fe07339f326b53c95",
    "182b27dbc8e8f36129ebef7d862432b6",
    "a698af80f2dab58fcf8125ee39599896",
    "765dc096fcb0600e63e5cac6dfcecae9",
    "0e71251df89b6b79d432a661bb698b12",
    "66eafc920154df0560ea5a14957d7121",
    "c5b62b4c62a2375399d18544d829670d",
    "4c0e057c28a45dc08ccc2edc0e10a6be",
    "a10202928e8ec00e9f589d20cf78a1a9",
    "43a7a89b0462def4cfe483c241c386d0",
    "05546b938db9090621291de40151b6cb",
    "29443d7a6f506ce953b18477c3b0f15c",
    "73c39de127a42601bbeabe9be5315448",
    "434895b1589fc41c14e6e391047a6e3a",
    "ce2d95a115369e048a9b94c2a16b4a0f",
    "b69b24387f0b8f16a019aef8ba7b0c03",
    "2e67df9269b8f16d20169289e20b811f",
    "21ab6aeb790dad1b6791997a4bb7d8f3",
    "7c77cca695b0219c728e7d92e96e660e",
    "a0e64f687090313ebf402d9d4e1ac0df",
    "9c052a6b24d6da14a677116b3782f279",
    "9db1b8d118f89b28346a1412a7616bd4",
    "4ebd45b8f2ad25b7c6df97450ee2af86",
    "f1a2b75e72dfeada26d8f99619b93f76",
    "463cf38a951f1bd4111d6b777aec5310",
    "97b4f64c6e462f824f17c741309d236a",
    "2fe0a190e4154e82d7f039dbcc13614b",
    "35cf8d630568e61b27b61070b52edb62",
    "a9b9a227e64b762688c15faf4be7a81d",
    "595a62ef2a9ce3de20e5ff92fd2045ff",
    "0b2b0abee13f7443212c25bdabdc4b56",
    "f9914118c83ea8bd79bfb94b82a34740",
    "11d1dbd45fa438e0c3cab13930a13109",
    "cba0e7f1083d117151acdc060822b67a",
    "14eb1fd9dda3b7ff1e677e15bb3ceff3",
    "89e3c32a57d950043c3330de8911cc91",
    "478a916c3e526e8e3c2cf7c853924f3d",
)

# 3 bands YCbCr, 8 bits.
_YCBCR_8BIT = (
    "14e5707616bb8fd5d0d65e098a13be75",
    "c3628e56a325fd81d39aead1f0d8cbbc",
    "55f1d71309f78a9358200bcfd3c36249",
    "56b53b106103016c58c346571694781c",
    "4753aa2b521807f38d36dd1a4514565d",
    "af05445541ef4dca66560c837a74417f",
    "10e397ea08a0f34fbd146b5fb70745d8",
    "1d94af42d0dcf6e05fb1478fe1d404fe",
    "0d2ae2b4c00891c5703b7676a35bb25b",
    "cda11d7b48b684b3591cfe0fc12aadd6",
    "590bf7c6c82ddf9fcfbce2387013aedc",
    "af1109e1eaa36360f78a85065aec4212",
    "51c1349d281b10f86638710652600b4e",
    "c6169d937af500eb3678f23070a53e71",
    "03f5e9be7d4e18d7e7630d3af50ab6d5",
    "be7345f6d85ce769ef95e6679e1182ff",
    "ebac68105888de0b1559bbfdb44153fe",
    "e67f62f10f728654412cbd0db3d9ef3a",
    "6b7e182ebc831b058d1351cd76ca5fde",
    "cbde6330d3913e16b6c2b4d1cebbeef2",
    "d67595ae924f5d02840c15df20c5a249",
    "c545a67b59762e27a5e7f566c9812f73",
    "0235a9e89ed0a3ac65e95a87d29c611e",
    "1592a9f85208bcd3cc00e87c77b887be",
    "315181de1dbb3df9b84a6ece5f0a22ce",
    "58ebdd6afa8cc14e37f7a57b71b8b8fa",
    "8bf710ffeeba78527fa6008bd3e7c1a0",
    "4feb2e4d2a042662a90ea21b11107dc1",
    "fecce20525708ff67fbc10728cf0dc75",
    "dc498543e263f3f6e1747dfef126ae7b",
    "eb7204cea3cff036eec0917e68d65d99",
    "cc2aaf55f44d10317032c672f331d89e",
    "48074c29fe44895df8175b4337ff3c57",
    "0a80d13c2d157ec2b55a767238d997d2",
    "9de8b780e038449d1c0aaf36cf62af4a",
    "e1875c51bf31de73712189c36f023974",
    "7626f31dda90023ce36649aaf446dfdf",
    "b25f604949d320f779a5911622cd5412",
    "160b92f5c94b2693553f4e5d0396de6b",
    "94989dc7fb7da09c7f688ae9a8e4bbfe",
    "14ed95069361a597eb39ef3128a862e9",
    "6c60224694780cd4066511d0e39d404d",
    "cc4ca9e2cbcdf98e53c8dc7d4207ef3e",
    "2d0612269677fd6369b321be7c919b76",
    "9e292f14aa88c8b0e4135f2d494d31f8",
    "ed26fc751997f272d266e6e8c9de520e",
    "edce19caeb2990348701642ac2e7eb79",
    "2165bbc2d77ede2ea6d7af94d63962ed",
    "cf2b4886a398d4d205c3ef2c4b9b476f",
    "e2df2906a8d3bebfe8db0022ad3fefb7",
    "e5addf1aa642a164760171ecfe642b23",
    "5aee55f61b1a0d8a1a28099b15a3487e",
    "c75507bc6161f108e13886f74aa57f96",
    "3387683bbd89a76e1264adb4cc9fe33f",
    "7523dcae5898c56deee1e7792d7f1bcb",
    "e87493f568670a56a5707d1fb941bd08",
    "c8c3643df89693b275e6e761ca91ceca",
    "70ba93b047ef64f2e3afdba7d1c09e69",
    "786cf16fc9235501bcaf886ac99c0c84",
    "5b9f5cd2e2c6cdf547008e5038504d11",
    "433713a1eadd82dc490c092594f6199d",
    "929e1d8f71744c0a521df674e2eb9bb3",
    "0dc9977561a812307f81480ab0109f0b",
    "7d6cc8b86844046f2718c2c243c4668a",
    "5e06bcbfeba331104aa1d5bfb5a40d66",
    "5d38d939b1c2678d6a8a6708ad91d937",
    "9fadcf58e6192091fef8c43265f2746c",
    "8e79ea522dce4333a8dfaaf29991350b",
    "a356461d43a00b22ea4d486c1bd3ece0",
    "fcf730a40f4963218baec5ebf9b2df6d",
    "d7ebd815e198ace5832ec281f0095476",
    "3f420cdf13aa02dfaf093ed00d1c90a2",
    "3bbe0fa9b9e23a88e4ed09e845e42f79",
    "0f53fc85df10e530c00bb55765906be7",
    "d1d1c2937f3d3ab8797f3ca289a39705",
    "f22822a4fc2b5b5a102e507552b59399",
    "4fdbb71af804f68ca8db0dd31d5862aa",
    "127069e7892b0f4061c8a763edd57964",
    "a84984ab641ed482814900b53d63ee3e",
    "fccfc6a235b0a7d4354764dc1ea99d29",
    "ab9b3a89d6deed60a449baeed1d82025",
    "d5159ddcd4b7e805148a275ddb171448",
    "438bf8f376179bd37dae623be336b0df",
    "62e617f3c7ffb44ba521dc920f7ead00",
    "5537ddea3a79c613bab95c5ced23d800",
    "ba4e5e5ddba82e76d18f1bf9016faba8",
    "b92849ade60b8493500ce78a270f4e30",
    "2c6ac3d1b80706b4c63af00ba7853a51",
    "bf963e57a863b82dd76df545ebefd8b7",
    "e73d5c4cc59e9a68c892cab97c8bf57e",
    "4d6fe6684875c811be4df97718e8c61c",
    "56e3d2b36568d77c2b8ffeb8fecbb33b",
    "e1668f617e81cdb12a571963fda68efa",
    "0a60c8049224f4f6e795d1934063b1b7",
    "024ea541602ccf6fbebf896af61925c2",
    "5ef21287a0ddcd9812763663bd22c9f6",
    "3a392ab4ec38d7b39cdb4f533217f96f",
    "cc0624dae2798e112762cee338b9a659",
    "55831b93c8f92bff09f91029bb6b8ae6",
    "75a5e8379c028e51b3d4cb19d57f5cd4",
)

# 3 bands YCbCr, 8 bits, libjpeg 9e's tables.
_YCBCR_8BIT_JPEG9E = (
    "0915b609019f595dc71811c1f5e9b873",
    "64a07aca404d22a858cb7feb586f789f",
    "61e9c802500df4c5b12a7aa1517b8026",
    "874921d232bf9f248b18551408af66e1",
    "2707f8d7aa21508e23d22822687d2691",
    "7b794cd1880acf30a22132bb029ef338",
    "16aac58f9413429a614839ae610c52ea",
    "30c03b0b5d0ae86583de12d9c2e32ddd",
    "aac437c808f7d0bf2a444330d2d04654",
    "b3d444691f29b4ee903e63afe4a65b52",
    "8999602cbf6dfac278e3ba447966db43",
    "1d7eecbc824df8b831c34f4c9c27f948",
    "308baca2c83817760c972f6e6f14fc46",
    "54d8110fc99a4116a27a698f1027aaf2",
    "b842bb0b054e4dea1340be2d347c9e53",
    "bb449674cac0677ad1cd48781846f015",
    "e1dd573c1470564dd71c66d2fa87ef77",
    "baf18ff54a81f353cbdc41517451d450",
    "5f62d36b46985336f185cd242aeef0a9",
    "bb000efa249587e41821c30bbcf46192",
    "d8c353150627a431fd5db324b8a586fc",
    "11c4007164baaa6b83877cb67eeee356",
    "939cb2056cccc791f13bb9bcf85c226f",
    "04b1b476f7d4733d157a6bdabea70716",
    "90fd542a834957dfe1375f261ec3cf7d",
    "117667ea71b8f98b699794c15fac981e",
    "b662528bd1e85e3551ea5c420f726267",
    "66ed9871d8b461490781f312115c9115",
    "df41882fd5980b63d2224ba85bcb77eb",
    "01b6ec0e48e7d577e1009d00bee2a182",
    "64151802afe2dcdef7bcf4467fbe418c",
    "db68f69d0af07222b71398ca797c2f8c",
    "6f94e9ddcc89f6ae29ee5f17d442c90a",
    "a03368e64d7f4a35b16243ac1183f115",
    "34e234dc6b1a61f5b9d43eda036a769d",
    "d0cceaa423a6766d10eda6691071ea35",
    "fa307ea4ac4c419ab35d2df175f063d0",
    "dcc787ea1270dce0f908a16e3b69da33",
    "04cd0275f680002ad2a2274b845f2169",
    "4c8df0a1b06efa55d035014be9868177",
    "37ae789a218894af0e38e06d8f8475c0",
    "3b96d5fe0f12f4737f457ed7ea3b3dc3",
    "a8460d779a4d45f307b49432a835b47f",
    "06c27a76bb39479c2669057efc438278",
    "8b91d70057001ff28b8b76f7f0ab43a7",
    "d54ee9b25da82f3342071c44b0c60ecf",
    "933b197b80ed81f09ebae15eb6344cc8",
    "0ba4505ad0d3f99428be8e72c73ab161",
    "4cf23130126aa06ea0344e74e5acf2d1",
    "4db26102bdbf54885ccb866e65d2086b",
    "7ec9886b0eba76e0298f3c8abee50708",
    "b5c13b41d68306085ce91e146503b919",
    "b8df76be92d452d9b1ade03e1f2f38c9",
    "8cee0bad5b647d3bca972ac4d1f61170",
    "c6abd06df3d20f2a2af2a8345a558afb",
    "9525f0b83ca084062ade7a5d0bb7726c",
    "013fddaa9ab7ea4e9b60c535693eb235",
    "931a5b2d24e37533ed610a7d9fea576a",
    "b155a0883e74fb2777883bd54df94b52",
    "9210ae6fa8987bad7fe24fa7cb67d34d",
    "1b426b74180f3857707b63cbbdac7532",
    "8a9d6c3e3342122ff9ce46656fbcec53",
    "423e330aec52b36a3141573eca3eb4c7",
    "7d6cc8b86844046f2718c2c243c4668a",
    "9e760db2032cf315793c18bb8ffe0093",
    "de1444357343710ff69d19143a3d71b0",
    "9fadcf58e6192091fef8c43265f2746c",
    "a585c4bf390f74868f9b6a2a3ac7f329",
    "5a28af3b3d62b29293da17dcbf7a2ab4",
    "fcf730a40f4963218baec5ebf9b2df6d",
    "93a0c9492beb22b9b4aa2158480eb2db",
    "5b7c75462009bd0c16c21d943f90154e",
    "3bbe0fa9b9e23a88e4ed09e845e42f79",
    "c1da2c9b41e7ac27015a65d523fa06f4",
    "20acc189183b13078acf18e2fefc85fa",
    "f22822a4fc2b5b5a102e507552b59399",
    "4a20577d695325d732d95e2eaacc981c",
    "127069e7892b0f4061c8a763edd57964",
    "a84984ab641ed482814900b53d63ee3e",
    "5734f5f6dcf476ec0ec9c97db371bde3",
    "ab9b3a89d6deed60a449baeed1d82025",
    "d5159ddcd4b7e805148a275ddb171448",
    "f0ec19cce0e94125f1ffbaaf196cbf4c",
    "62e617f3c7ffb44ba521dc920f7ead00",
    "5537ddea3a79c613bab95c5ced23d800",
    "3033af62b7c89a87289e6ef596640460",
    "b92849ade60b8493500ce78a270f4e30",
    "2c6ac3d1b80706b4c63af00ba7853a51",
    "bf963e57a863b82dd76df545ebefd8b7",
    "e73d5c4cc59e9a68c892cab97c8bf57e",
    "4d6fe6684875c811be4df97718e8c61c",
    "56e3d2b36568d77c2b8ffeb8fecbb33b",
    "e1668f617e81cdb12a571963fda68efa",
    "0a60c8049224f4f6e795d1934063b1b7",
    "024ea541602ccf6fbebf896af61925c2",
    "5ef21287a0ddcd9812763663bd22c9f6",
    "3a392ab4ec38d7b39cdb4f533217f96f",
    "cc0624dae2798e112762cee338b9a659",
    "55831b93c8f92bff09f91029bb6b8ae6",
    "75a5e8379c028e51b3d4cb19d57f5cd4",
)


def _next_table(data: bytes, marker: int, start: int) -> tuple[int, int] | None:
    """The next JPEG marker segment of a kind, as GDAL's `GTIFFFindNextTable` walks them.

    Args:
        data: The `JPEGTables` bytes.
        marker: The marker (`0xDB` quantization, `0xC4` Huffman).
        start: Where to start.

    Returns:
        tuple | None: `(offset, length)` of the segment after its marker byte (its length
        field included), or None.
    """
    n = len(data) - start
    i = 0
    while i + 1 < n:
        if data[start + i] != 0xFF:
            return None
        i += 1
        if data[start + i] == 0xD8:
            i += 1
            continue
        if i + 2 >= n:
            return None
        length = data[start + i + 1] * 256 + data[start + i + 2]
        if i + 1 + length >= n:
            return None
        if data[start + i] == marker:
            return start + i + 1, length
        i += 1 + length
    return None


def _quality_from_md5(sums: tuple[str, ...], data: bytes) -> int:
    """The quality whose quantization tables have the MD5 sum of the file's, or -1."""
    md5 = hashlib.md5(usedforsecurity=False)
    cur = 0
    while (found := _next_table(data, _QUANT_TABLE, cur)) is not None:
        offset, length = found
        md5.update(data[offset : offset + length])
        cur = offset + length
    digest = md5.hexdigest()
    return sums.index(digest) + 1 if digest in sums else -1


def guess_jpeg_quality(
    tables: bytes, bands: int, bits: int, photometric: int
) -> tuple[int, bool, bool]:
    """The JPEG quality GDAL reports for a JPEG-compressed TIFF.

    Args:
        tables: The `JPEGTables` tag.
        bands: The number of bands.
        bits: The bits per sample.
        photometric: The photometric interpretation.

    Returns:
        tuple: `(quality, has_quantization_tables, has_huffman_tables)`; the quality is -1
        when GDAL's tables do not give it.
    """
    has_quant = _next_table(tables, _QUANT_TABLE, 0) is not None
    has_huffman = _next_table(tables, _HUFFMAN_TABLE, 0) is not None
    if not has_quant or bits != 8:
        return -1, has_quant, has_huffman
    if bands == 3 and photometric == 6:
        quality = _quality_from_md5(_YCBCR_8BIT, tables)
        if quality < 0:
            quality = _quality_from_md5(_YCBCR_8BIT_JPEG9E, tables)
        return quality, has_quant, has_huffman
    if photometric != 6:
        # One band, RGB, CMYK (GDAL's sums), and the layouts GDAL tries at every quality: libtiff
        # writes the luminance table only unless the image is YCbCr (`prepare_JPEGTables`).
        return _quality_from_md5(_GENERIC_8BIT, tables), has_quant, has_huffman
    return -1, has_quant, has_huffman
