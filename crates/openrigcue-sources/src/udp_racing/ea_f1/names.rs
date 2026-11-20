//! Team and track names of the F1 2019–2025 games, by id.

/// Look up `id` in a sorted id table; `None` for an unknown id.
pub(super) fn lookup<K: Ord + Copy>(table: &[(K, &'static str)], id: K) -> Option<&'static str> {
    table.binary_search_by(|(key, _)| key.cmp(&id)).ok().map(|index| table[index].1)
}

/// F1 2019 teams (team id -> team name), sorted by id.
#[rustfmt::skip]
pub(super) const TEAMS_2019: &[(u8, &str)] = &[
    (0, "Mercedes"), (1, "Ferrari"), (2, "Red Bull Racing"), (3, "Williams"),
    (4, "Racing Point"), (5, "Renault"), (6, "Toro Rosso"), (7, "Haas"),
    (8, "McLaren"), (9, "Alfa Romeo"), (10, "McLaren 1988"), (11, "McLaren 1991"),
    (12, "Williams 1992"), (13, "Ferrari 1995"), (14, "Williams 1996"), (15, "McLaren 1998"),
    (16, "Ferrari 2002"), (17, "Ferrari 2004"), (18, "Renault 2006"), (19, "Ferrari 2007"),
    (21, "Red Bull 2010"), (22, "Ferrari 1976"), (23, "ART Grand Prix"), (24, "Campos Vexatec Racing"),
    (25, "Carlin"), (26, "Charouz Racing System"), (27, "DAMS"), (28, "Russian Time"),
    (29, "MP Motorsport"), (30, "Pertamina"), (31, "McLaren 1990"), (32, "Trident"),
    (33, "BWT Arden"), (34, "McLaren 1976"), (35, "Lotus 1972"), (36, "Ferrari 1979"),
    (37, "McLaren 1982"), (38, "Williams 2003"), (39, "Brawn 2009"), (40, "Lotus 1978"),
    (63, "Ferrari 1990"), (64, "McLaren 2010"), (65, "Ferrari 2010"),
];

/// F1 2019 tracks (track id -> track name), sorted by id.
#[rustfmt::skip]
pub(super) const TRACKS_2019: &[(i8, &str)] = &[
    (0, "Melbourne"), (1, "Paul Ricard"), (2, "Shanghai"), (3, "Sakhir (Bahrain)"),
    (4, "Catalunya"), (5, "Monaco"), (6, "Montreal"), (7, "Silverstone"),
    (8, "Hockenheim"), (9, "Hungaroring"), (10, "Spa"), (11, "Monza"),
    (12, "Singapore"), (13, "Suzuka"), (14, "Abu Dhabi"), (15, "Texas"),
    (16, "Brazil"), (17, "Austria"), (18, "Sochi"), (19, "Mexico"),
    (20, "Baku (Azerbaijan)"), (21, "Sakhir Short"), (22, "Silverstone Short"), (23, "Texas Short"),
    (24, "Suzuka Short"),
];

/// F1 2020 teams (team id -> team name), sorted by id.
#[rustfmt::skip]
pub(super) const TEAMS_2020: &[(u8, &str)] = &[
    (0, "Mercedes"), (1, "Ferrari"), (2, "Red Bull Racing"), (3, "Williams"),
    (4, "Racing Point"), (5, "Renault"), (6, "AlphaTauri"), (7, "Haas"),
    (8, "McLaren"), (9, "Alfa Romeo"), (10, "McLaren 1988"), (11, "McLaren 1991"),
    (12, "Williams 1992"), (13, "Ferrari 1995"), (14, "Williams 1996"), (15, "McLaren 1998"),
    (16, "Ferrari 2002"), (17, "Ferrari 2004"), (18, "Renault 2006"), (19, "Ferrari 2007"),
    (21, "Red Bull 2010"), (31, "McLaren 1990"), (38, "Williams 2003"), (39, "Brawn 2009"),
    (41, "F1 Generic car"), (42, "ART Grand Prix"), (43, "Campos Racing"), (44, "Carlin"),
    (45, "Sauber Junior Team by Charouz"), (46, "DAMS"), (47, "UNI-Virtuosi"), (48, "MP Motorsport"),
    (49, "PREMA Racing"), (50, "Trident"), (51, "BWT Arden"), (53, "Benetton 1994"),
    (54, "Benetton 1995"), (55, "Ferrari 2000"), (56, "Jordan 1991"), (63, "Ferrari 1990"),
    (64, "McLaren 2010"), (65, "Ferrari 2010"), (255, "My Team"),
];

/// F1 2020 tracks (track id -> track name), sorted by id.
#[rustfmt::skip]
pub(super) const TRACKS_2020: &[(i8, &str)] = &[
    (0, "Melbourne"), (1, "Paul Ricard"), (2, "Shanghai"), (3, "Sakhir (Bahrain)"),
    (4, "Catalunya"), (5, "Monaco"), (6, "Montreal"), (7, "Silverstone"),
    (9, "Hungaroring"), (10, "Spa"), (11, "Monza"), (12, "Singapore"),
    (13, "Suzuka"), (14, "Abu Dhabi"), (15, "Texas"), (16, "Brazil"),
    (17, "Austria"), (18, "Sochi"), (19, "Mexico"), (20, "Baku (Azerbaijan)"),
    (21, "Sakhir Short"), (22, "Silverstone Short"), (23, "Texas Short"), (24, "Suzuka Short"),
    (25, "Hanoi"), (26, "Zandvoort"),
];

/// F1 2021 teams (team id -> team name), sorted by id.
#[rustfmt::skip]
pub(super) const TEAMS_2021: &[(u8, &str)] = &[
    (0, "Mercedes"), (1, "Ferrari"), (2, "Red Bull Racing"), (3, "Williams"),
    (4, "Aston Martin"), (5, "Alpine"), (6, "Alpha Tauri"), (7, "Haas"),
    (8, "McLaren"), (9, "Alfa Romeo"), (42, "Art GP ’19"), (43, "Campos ’19"),
    (44, "Carlin ’19"), (45, "Sauber Junior Charouz ’19"), (46, "Dams ’19"), (47, "Uni-Virtuosi ‘19"),
    (48, "MP Motorsport ‘19"), (49, "Prema ’19"), (50, "Trident ’19"), (51, "Arden ’19"),
    (70, "Art GP ‘20"), (71, "Campos ‘20"), (72, "Carlin ‘20"), (73, "Charouz ‘20"),
    (74, "Dams ‘20"), (75, "Uni-Virtuosi ‘20"), (76, "MP Motorsport ‘20"), (77, "Prema ‘20"),
    (78, "Trident ‘20"), (79, "BWT ‘20"), (80, "Hitech ‘20"), (85, "Mercedes 2020"),
    (86, "Ferrari 2020"), (87, "Red Bull 2020"), (88, "Williams 2020"), (89, "Racing Point 2020"),
    (90, "Renault 2020"), (91, "Alpha Tauri 2020"), (92, "Haas 2020"), (93, "McLaren 2020"),
    (94, "Alfa Romeo 2020"), (255, "My Team"),
];

/// F1 2021 tracks (track id -> track name), sorted by id.
#[rustfmt::skip]
pub(super) const TRACKS_2021: &[(i8, &str)] = &[
    (0, "Melbourne"), (1, "Paul Ricard"), (2, "Shanghai"), (3, "Sakhir (Bahrain)"),
    (4, "Catalunya"), (5, "Monaco"), (6, "Montreal"), (7, "Silverstone"),
    (8, "Hockenheim"), (9, "Hungaroring"), (10, "Spa"), (11, "Monza"),
    (12, "Singapore"), (13, "Suzuka"), (14, "Abu Dhabi"), (15, "Texas"),
    (16, "Brazil"), (17, "Austria"), (18, "Sochi"), (19, "Mexico"),
    (20, "Baku (Azerbaijan)"), (21, "Sakhir Short"), (22, "Silverstone Short"), (23, "Texas Short"),
    (24, "Suzuka Short"), (25, "Hanoi"), (26, "Zandvoort"), (27, "Imola"),
    (28, "Portimão"), (29, "Jeddah"),
];

/// F1 22 teams (team id -> team name), sorted by id.
#[rustfmt::skip]
pub(super) const TEAMS_2022: &[(u8, &str)] = &[
    (0, "Mercedes"), (1, "Ferrari"), (2, "Red Bull Racing"), (3, "Williams"),
    (4, "Aston Martin"), (5, "Alpine"), (6, "Alpha Tauri"), (7, "Haas"),
    (8, "McLaren"), (9, "Alfa Romeo"), (41, "Multiplayer"), (85, "Mercedes 2020"),
    (86, "Ferrari 2020"), (87, "Red Bull 2020"), (88, "Williams 2020"), (89, "Racing Point 2020"),
    (90, "Renault 2020"), (91, "Alpha Tauri 2020"), (92, "Haas 2020"), (93, "McLaren 2020"),
    (94, "Alfa Romeo 2020"), (95, "Aston Martin DB11 V12"), (96, "Aston Martin Vantage F1 Edition"), (97, "Aston Martin Vantage Safety Car"),
    (98, "Ferrari F8 Tributo"), (99, "Ferrari Roma"), (100, "McLaren 720S"), (101, "McLaren Artura"),
    (102, "Mercedes AMG GT Black Series Safety Car"), (103, "Mercedes AMG GTR Pro"), (104, "F1 Custom Team"), (106, "Prema ‘21"),
    (107, "Uni-Virtuosi ‘21"), (108, "Carlin ‘21"), (109, "Hitech ‘21"), (110, "Art GP ‘21"),
    (111, "MP Motorsport ‘21"), (112, "Charouz ‘21"), (113, "Dams ‘21"), (114, "Campos ‘21"),
    (115, "BWT ‘21"), (116, "Trident ‘21"), (117, "Mercedes AMG GT Black Series91Alpha Tauri 2020"), (118, "Prema ‘22"),
    (119, "Virtuosi ‘22"), (120, "Carlin ‘22"), (121, "Hitech ‘22"), (122, "Art GP ‘22"),
    (123, "MP Motorsport ‘22"), (124, "Charouz ‘22"), (125, "Dams ‘22"), (126, "Campos ‘22"),
    (127, "Van Amersfoort Racing ‘22"), (128, "Trident ‘22"), (255, "My Team"),
];

/// F1 22 tracks (track id -> track name), sorted by id.
#[rustfmt::skip]
pub(super) const TRACKS_2022: &[(i8, &str)] = &[
    (0, "Melbourne"), (1, "Paul Ricard"), (2, "Shanghai"), (3, "Sakhir (Bahrain)"),
    (4, "Catalunya"), (5, "Monaco"), (6, "Montreal"), (7, "Silverstone"),
    (8, "Hockenheim"), (9, "Hungaroring"), (10, "Spa"), (11, "Monza"),
    (12, "Singapore"), (13, "Suzuka"), (14, "Abu Dhabi"), (15, "Texas"),
    (16, "Brazil"), (17, "Austria"), (18, "Sochi"), (19, "Mexico"),
    (20, "Baku (Azerbaijan)"), (21, "Sakhir Short"), (22, "Silverstone Short"), (23, "Texas Short"),
    (24, "Suzuka Short"), (25, "Hanoi"), (26, "Zandvoort"), (27, "Imola"),
    (28, "Portimão"), (29, "Jeddah"), (30, "Miami"),
];

/// F1 23 teams (team id -> team name), sorted by id.
#[rustfmt::skip]
pub(super) const TEAMS_2023: &[(u8, &str)] = &[
    (0, "Mercedes"), (1, "Ferrari"), (2, "Red Bull Racing"), (3, "Williams"),
    (4, "Aston Martin"), (5, "Alpine"), (6, "Alpha Tauri"), (7, "Haas"),
    (8, "McLaren"), (9, "Alfa Romeo"), (10, "Unknown"), (11, "Unknown"),
    (12, "Unknown"), (13, "Unknown"), (14, "Unknown"), (15, "Unknown"),
    (16, "Unknown"), (17, "Unknown"), (18, "Unknown"), (19, "Unknown"),
    (20, "Unknown"), (21, "Unknown"), (22, "Unknown"), (23, "Unknown"),
    (24, "Unknown"), (25, "Unknown"), (26, "Unknown"), (27, "Unknown"),
    (28, "Unknown"), (29, "Unknown"), (30, "Unknown"), (31, "Unknown"),
    (32, "Unknown"), (33, "Unknown"), (34, "Unknown"), (35, "Unknown"),
    (36, "Unknown"), (37, "Unknown"), (38, "Unknown"), (39, "Unknown"),
    (40, "Unknown"), (41, "Unknown"), (42, "Unknown"), (43, "Unknown"),
    (44, "Unknown"), (45, "Unknown"), (46, "Unknown"), (47, "Unknown"),
    (48, "Unknown"), (49, "Unknown"), (50, "Unknown"), (51, "Unknown"),
    (52, "Unknown"), (53, "Unknown"), (54, "Unknown"), (55, "Unknown"),
    (56, "Unknown"), (57, "Unknown"), (58, "Unknown"), (59, "Unknown"),
    (60, "Unknown"), (61, "Unknown"), (62, "Unknown"), (63, "Unknown"),
    (64, "Unknown"), (65, "Unknown"), (66, "Unknown"), (67, "Unknown"),
    (68, "Unknown"), (69, "Unknown"), (70, "Unknown"), (71, "Unknown"),
    (72, "Unknown"), (73, "Unknown"), (74, "Unknown"), (75, "Unknown"),
    (76, "Unknown"), (77, "Unknown"), (78, "Unknown"), (79, "Unknown"),
    (80, "Unknown"), (81, "Unknown"), (82, "Unknown"), (83, "Unknown"),
    (84, "Unknown"), (85, "Mercedes 2020"), (86, "Ferrari 2020"), (87, "Red Bull 2020"),
    (88, "Williams 2020"), (89, "Racing Point 2020"), (90, "Renault 2020"), (91, "Alpha Tauri 2020"),
    (92, "Haas 2020"), (93, "McLaren 2020"), (94, "Alfa Romeo 2020"), (95, "Aston Martin DB11 V12"),
    (96, "Aston Martin Vantage F1 Edition"), (97, "Aston Martin Vantage Safety Car"), (98, "Ferrari F8 Tributo"), (99, "Ferrari Roma"),
    (100, "McLaren 720S"), (101, "McLaren Artura"), (102, "Mercedes AMG GT Black Series Safety Car"), (103, "Mercedes AMG GTR Pro"),
    (104, "F1 Custom Team"), (106, "Prema ‘21"), (107, "Uni-Virtuosi ‘21"), (108, "Carlin ‘21"),
    (109, "Hitech ‘21"), (110, "Art GP ‘21"), (111, "MP Motorsport ‘21"), (112, "Charouz ‘21"),
    (113, "Dams ‘21"), (114, "Campos ‘21"), (115, "BWT ‘21"), (116, "Trident ‘21"),
    (117, "Mercedes AMG GT Black Series"), (118, "Mercedes ‘22"), (119, "Ferrari ‘22"), (120, "Red Bull Racing ‘22"),
    (121, "Williams ‘22"), (122, "Aston Martin ‘22"), (123, "Alpine ‘22"), (124, "Alpha Tauri ‘22"),
    (125, "Haas ‘22"), (126, "McLaren ‘22"), (127, "Alpha Romeo ‘22"), (128, "Konnersport ‘22"),
    (129, "Konnersport"), (130, "Prema ‘22"), (131, "Virtuosi ‘22"), (132, "Carlin ‘22"),
    (133, "MP Motorsport ‘22"), (134, "Charouz ‘22"), (135, "Dams ‘22"), (136, "Campos ‘22"),
    (137, "Van Amersfoort Racing ‘22"), (138, "Trident ‘22"), (139, "Hitech ‘22"), (140, "Art GP ‘22"),
    (141, "Unknown"), (142, "Unknown"), (143, "Unknown"), (144, "Unknown"),
    (145, "Unknown"), (146, "Unknown"), (147, "Unknown"), (148, "Unknown"),
    (149, "Unknown"), (150, "Unknown"), (151, "Unknown"), (152, "Unknown"),
    (153, "Unknown"), (154, "Unknown"), (155, "Unknown"), (156, "Unknown"),
    (157, "Unknown"), (158, "Unknown"), (159, "Unknown"), (160, "Unknown"),
    (161, "Unknown"), (162, "Unknown"), (163, "Unknown"), (164, "Unknown"),
    (165, "Unknown"), (166, "Unknown"), (167, "Unknown"), (168, "Unknown"),
    (169, "Unknown"), (170, "Unknown"), (171, "Unknown"), (172, "Unknown"),
    (173, "Unknown"), (174, "Unknown"), (175, "Unknown"), (176, "Unknown"),
    (177, "Unknown"), (178, "Unknown"), (179, "Unknown"), (180, "Unknown"),
    (181, "Unknown"), (182, "Unknown"), (183, "Unknown"), (184, "Unknown"),
    (185, "Unknown"), (186, "Unknown"), (187, "Unknown"), (188, "Unknown"),
    (189, "Unknown"), (190, "Unknown"), (191, "Unknown"), (192, "Unknown"),
    (193, "Unknown"), (194, "Unknown"), (195, "Unknown"), (196, "Unknown"),
    (197, "Unknown"), (198, "Unknown"), (199, "Unknown"), (200, "Unknown"),
    (201, "Unknown"), (202, "Unknown"), (203, "Unknown"), (204, "Unknown"),
    (205, "Unknown"), (206, "Unknown"), (207, "Unknown"), (208, "Unknown"),
    (209, "Unknown"), (210, "Unknown"), (211, "Unknown"), (212, "Unknown"),
    (213, "Unknown"), (214, "Unknown"), (215, "Unknown"), (216, "Unknown"),
    (217, "Unknown"), (218, "Unknown"), (219, "Unknown"), (220, "Unknown"),
    (221, "Unknown"), (222, "Unknown"), (223, "Unknown"), (224, "Unknown"),
    (225, "Unknown"), (226, "Unknown"), (227, "Unknown"), (228, "Unknown"),
    (229, "Unknown"), (230, "Unknown"), (231, "Unknown"), (232, "Unknown"),
    (233, "Unknown"), (234, "Unknown"), (235, "Unknown"), (236, "Unknown"),
    (237, "Unknown"), (238, "Unknown"), (239, "Unknown"), (240, "Unknown"),
    (241, "Unknown"), (242, "Unknown"), (243, "Unknown"), (244, "Unknown"),
    (245, "Unknown"), (246, "Unknown"), (247, "Unknown"), (248, "Unknown"),
    (249, "Unknown"), (250, "Unknown"), (251, "Unknown"), (252, "Unknown"),
    (253, "Unknown"), (254, "Unknown"), (255, "My Team"),
];

/// F1 23 tracks (track id -> track name), sorted by id.
#[rustfmt::skip]
pub(super) const TRACKS_2023: &[(i8, &str)] = &[
    (0, "Melbourne"), (1, "Paul Ricard"), (2, "Shanghai"), (3, "Sakhir (Bahrain)"),
    (4, "Catalunya"), (5, "Monaco"), (6, "Montreal"), (7, "Silverstone"),
    (8, "Hockenheim"), (9, "Hungaroring"), (10, "Spa"), (11, "Monza"),
    (12, "Singapore"), (13, "Suzuka"), (14, "Abu Dhabi"), (15, "Texas"),
    (16, "Brazil"), (17, "Austria"), (18, "Sochi"), (19, "Mexico"),
    (20, "Baku (Azerbaijan)"), (21, "Sakhir Short"), (22, "Silverstone Short"), (23, "Texas Short"),
    (24, "Suzuka Short"), (25, "Hanoi"), (26, "Zandvoort"), (27, "Imola"),
    (28, "Portimão"), (29, "Jeddah"), (30, "Miami"), (31, "Las Vegas"),
    (32, "Losail"),
];

/// F1 24 teams (team id -> team name), sorted by id.
#[rustfmt::skip]
pub(super) const TEAMS_2024: &[(u8, &str)] = &[
    (0, "Mercedes"), (1, "Ferrari"), (2, "Red Bull Racing"), (3, "Williams"),
    (4, "Aston Martin"), (5, "Alpine"), (6, "RB"), (7, "Haas"),
    (8, "McLaren"), (9, "Sauber"), (10, "Unknown"), (11, "Unknown"),
    (12, "Unknown"), (13, "Unknown"), (14, "Unknown"), (15, "Unknown"),
    (16, "Unknown"), (17, "Unknown"), (18, "Unknown"), (19, "Unknown"),
    (20, "Unknown"), (21, "Unknown"), (22, "Unknown"), (23, "Unknown"),
    (24, "Unknown"), (25, "Unknown"), (26, "Unknown"), (27, "Unknown"),
    (28, "Unknown"), (29, "Unknown"), (30, "Unknown"), (31, "Unknown"),
    (32, "Unknown"), (33, "Unknown"), (34, "Unknown"), (35, "Unknown"),
    (36, "Unknown"), (37, "Unknown"), (38, "Unknown"), (39, "Unknown"),
    (40, "Unknown"), (41, "F1 Generic"), (42, "Unknown"), (43, "Unknown"),
    (44, "Unknown"), (45, "Unknown"), (46, "Unknown"), (47, "Unknown"),
    (48, "Unknown"), (49, "Unknown"), (50, "Unknown"), (51, "Unknown"),
    (52, "Unknown"), (53, "Unknown"), (54, "Unknown"), (55, "Unknown"),
    (56, "Unknown"), (57, "Unknown"), (58, "Unknown"), (59, "Unknown"),
    (60, "Unknown"), (61, "Unknown"), (62, "Unknown"), (63, "Unknown"),
    (64, "Unknown"), (65, "Unknown"), (66, "Unknown"), (67, "Unknown"),
    (68, "Unknown"), (69, "Unknown"), (70, "Unknown"), (71, "Unknown"),
    (72, "Unknown"), (73, "Unknown"), (74, "Unknown"), (75, "Unknown"),
    (76, "Unknown"), (77, "Unknown"), (78, "Unknown"), (79, "Unknown"),
    (80, "Unknown"), (81, "Unknown"), (82, "Unknown"), (83, "Unknown"),
    (84, "Unknown"), (85, "Unknown"), (86, "Unknown"), (87, "Unknown"),
    (88, "Unknown"), (89, "Unknown"), (90, "Unknown"), (91, "Unknown"),
    (92, "Unknown"), (93, "Unknown"), (94, "Unknown"), (95, "Unknown"),
    (96, "Unknown"), (97, "Unknown"), (98, "Unknown"), (99, "Unknown"),
    (100, "Unknown"), (101, "Unknown"), (102, "Unknown"), (103, "Unknown"),
    (104, "F1 Custom Team"), (106, "Unknown"), (107, "Unknown"), (108, "Unknown"),
    (109, "Unknown"), (110, "Unknown"), (111, "Unknown"), (112, "Unknown"),
    (113, "Unknown"), (114, "Unknown"), (115, "Unknown"), (116, "Unknown"),
    (117, "Unknown"), (118, "Unknown"), (119, "Unknown"), (120, "Unknown"),
    (121, "Unknown"), (122, "Unknown"), (123, "Unknown"), (124, "Unknown"),
    (125, "Unknown"), (126, "Unknown"), (127, "Unknown"), (128, "Unknown"),
    (129, "Unknown"), (130, "Unknown"), (131, "Unknown"), (132, "Unknown"),
    (133, "Unknown"), (134, "Unknown"), (135, "Unknown"), (136, "Unknown"),
    (137, "Unknown"), (138, "Unknown"), (139, "Unknown"), (140, "Unknown"),
    (141, "Unknown"), (142, "Unknown"), (143, "Art GP ‘23"), (144, "Campos ‘23"),
    (145, "Carlin ‘23"), (146, "PHM ‘23"), (147, "Dams ‘23"), (148, "Hitech ‘23"),
    (149, "MP Motorsport ‘23"), (150, "Prema ‘23"), (151, "Trident ‘23"), (152, "Van Amersfoort Racing ‘23"),
    (153, "Virtuosi ‘23"), (154, "Unknown"), (155, "Unknown"), (156, "Unknown"),
    (157, "Unknown"), (158, "Unknown"), (159, "Unknown"), (160, "Unknown"),
    (161, "Unknown"), (162, "Unknown"), (163, "Unknown"), (164, "Unknown"),
    (165, "Unknown"), (166, "Unknown"), (167, "Unknown"), (168, "Unknown"),
    (169, "Unknown"), (170, "Unknown"), (171, "Unknown"), (172, "Unknown"),
    (173, "Unknown"), (174, "Unknown"), (175, "Unknown"), (176, "Unknown"),
    (177, "Unknown"), (178, "Unknown"), (179, "Unknown"), (180, "Unknown"),
    (181, "Unknown"), (182, "Unknown"), (183, "Unknown"), (184, "Unknown"),
    (185, "Unknown"), (186, "Unknown"), (187, "Unknown"), (188, "Unknown"),
    (189, "Unknown"), (190, "Unknown"), (191, "Unknown"), (192, "Unknown"),
    (193, "Unknown"), (194, "Unknown"), (195, "Unknown"), (196, "Unknown"),
    (197, "Unknown"), (198, "Unknown"), (199, "Unknown"), (200, "Unknown"),
    (201, "Unknown"), (202, "Unknown"), (203, "Unknown"), (204, "Unknown"),
    (205, "Unknown"), (206, "Unknown"), (207, "Unknown"), (208, "Unknown"),
    (209, "Unknown"), (210, "Unknown"), (211, "Unknown"), (212, "Unknown"),
    (213, "Unknown"), (214, "Unknown"), (215, "Unknown"), (216, "Unknown"),
    (217, "Unknown"), (218, "Unknown"), (219, "Unknown"), (220, "Unknown"),
    (221, "Unknown"), (222, "Unknown"), (223, "Unknown"), (224, "Unknown"),
    (225, "Unknown"), (226, "Unknown"), (227, "Unknown"), (228, "Unknown"),
    (229, "Unknown"), (230, "Unknown"), (231, "Unknown"), (232, "Unknown"),
    (233, "Unknown"), (234, "Unknown"), (235, "Unknown"), (236, "Unknown"),
    (237, "Unknown"), (238, "Unknown"), (239, "Unknown"), (240, "Unknown"),
    (241, "Unknown"), (242, "Unknown"), (243, "Unknown"), (244, "Unknown"),
    (245, "Unknown"), (246, "Unknown"), (247, "Unknown"), (248, "Unknown"),
    (249, "Unknown"), (250, "Unknown"), (251, "Unknown"), (252, "Unknown"),
    (253, "Unknown"), (254, "Unknown"), (255, "My Team"),
];

/// F1 24 tracks (track id -> track name), sorted by id.
#[rustfmt::skip]
pub(super) const TRACKS_2024: &[(i8, &str)] = &[
    (0, "Melbourne"), (1, "Paul Ricard"), (2, "Shanghai"), (3, "Sakhir (Bahrain)"),
    (4, "Catalunya"), (5, "Monaco"), (6, "Montreal"), (7, "Silverstone"),
    (8, "Hockenheim"), (9, "Hungaroring"), (10, "Spa"), (11, "Monza"),
    (12, "Singapore"), (13, "Suzuka"), (14, "Abu Dhabi"), (15, "Texas"),
    (16, "Brazil"), (17, "Austria"), (18, "Sochi"), (19, "Mexico"),
    (20, "Baku (Azerbaijan)"), (21, "Sakhir Short"), (22, "Silverstone Short"), (23, "Texas Short"),
    (24, "Suzuka Short"), (25, "Hanoi"), (26, "Zandvoort"), (27, "Imola"),
    (28, "Portimão"), (29, "Jeddah"), (30, "Miami"), (31, "Las Vegas"),
    (32, "Losail"),
];

/// F1 25 teams (team id -> team name), sorted by id.
#[rustfmt::skip]
pub(super) const TEAMS_2025: &[(u8, &str)] = &[
    (0, "Mercedes"), (1, "Ferrari"), (2, "Red Bull Racing"), (3, "Williams"),
    (4, "Aston Martin"), (5, "Alpine"), (6, "RB"), (7, "Haas"),
    (8, "McLaren"), (9, "Sauber"), (10, "Unknown"), (11, "Unknown"),
    (12, "Unknown"), (13, "Unknown"), (14, "Unknown"), (15, "Unknown"),
    (16, "Unknown"), (17, "Unknown"), (18, "Unknown"), (19, "Unknown"),
    (20, "Unknown"), (21, "Unknown"), (22, "Unknown"), (23, "Unknown"),
    (24, "Unknown"), (25, "Unknown"), (26, "Unknown"), (27, "Unknown"),
    (28, "Unknown"), (29, "Unknown"), (30, "Unknown"), (31, "Unknown"),
    (32, "Unknown"), (33, "Unknown"), (34, "Unknown"), (35, "Unknown"),
    (36, "Unknown"), (37, "Unknown"), (38, "Unknown"), (39, "Unknown"),
    (40, "Unknown"), (41, "F1 Generic"), (42, "Unknown"), (43, "Unknown"),
    (44, "Unknown"), (45, "Unknown"), (46, "Unknown"), (47, "Unknown"),
    (48, "Unknown"), (49, "Unknown"), (50, "Unknown"), (51, "Unknown"),
    (52, "Unknown"), (53, "Unknown"), (54, "Unknown"), (55, "Unknown"),
    (56, "Unknown"), (57, "Unknown"), (58, "Unknown"), (59, "Unknown"),
    (60, "Unknown"), (61, "Unknown"), (62, "Unknown"), (63, "Unknown"),
    (64, "Unknown"), (65, "Unknown"), (66, "Unknown"), (67, "Unknown"),
    (68, "Unknown"), (69, "Unknown"), (70, "Unknown"), (71, "Unknown"),
    (72, "Unknown"), (73, "Unknown"), (74, "Unknown"), (75, "Unknown"),
    (76, "Unknown"), (77, "Unknown"), (78, "Unknown"), (79, "Unknown"),
    (80, "Unknown"), (81, "Unknown"), (82, "Unknown"), (83, "Unknown"),
    (84, "Unknown"), (85, "Unknown"), (86, "Unknown"), (87, "Unknown"),
    (88, "Unknown"), (89, "Unknown"), (90, "Unknown"), (91, "Unknown"),
    (92, "Unknown"), (93, "Unknown"), (94, "Unknown"), (95, "Unknown"),
    (96, "Unknown"), (97, "Unknown"), (98, "Unknown"), (99, "Unknown"),
    (100, "Unknown"), (101, "Unknown"), (102, "Unknown"), (103, "Unknown"),
    (104, "F1 Custom Team"), (106, "Unknown"), (107, "Unknown"), (108, "Unknown"),
    (109, "Unknown"), (110, "Unknown"), (111, "Unknown"), (112, "Unknown"),
    (113, "Unknown"), (114, "Unknown"), (115, "Unknown"), (116, "Unknown"),
    (117, "Unknown"), (118, "Unknown"), (119, "Unknown"), (120, "Unknown"),
    (121, "Unknown"), (122, "Unknown"), (123, "Unknown"), (124, "Unknown"),
    (125, "Unknown"), (126, "Unknown"), (127, "Unknown"), (128, "Unknown"),
    (129, "Unknown"), (130, "Unknown"), (131, "Unknown"), (132, "Unknown"),
    (133, "Unknown"), (134, "Unknown"), (135, "Unknown"), (136, "Unknown"),
    (137, "Unknown"), (138, "Unknown"), (139, "Unknown"), (140, "Unknown"),
    (141, "Unknown"), (142, "Unknown"), (143, "Art GP ‘23"), (144, "Campos ‘23"),
    (145, "Carlin ‘23"), (146, "PHM ‘23"), (147, "Dams ‘23"), (148, "Hitech ‘23"),
    (149, "MP Motorsport ‘23"), (150, "Prema ‘23"), (151, "Trident ‘23"), (152, "Van Amersfoort Racing ‘23"),
    (153, "Virtuosi ‘23"), (154, "Unknown"), (155, "Unknown"), (156, "Unknown"),
    (157, "Unknown"), (158, "Unknown"), (159, "Unknown"), (160, "Unknown"),
    (161, "Unknown"), (162, "Unknown"), (163, "Unknown"), (164, "Unknown"),
    (165, "Unknown"), (166, "Unknown"), (167, "Unknown"), (168, "Unknown"),
    (169, "Unknown"), (170, "Unknown"), (171, "Unknown"), (172, "Unknown"),
    (173, "Unknown"), (174, "Unknown"), (175, "Unknown"), (176, "Unknown"),
    (177, "Unknown"), (178, "Unknown"), (179, "Unknown"), (180, "Unknown"),
    (181, "Unknown"), (182, "Unknown"), (183, "Unknown"), (184, "Unknown"),
    (185, "Unknown"), (186, "Unknown"), (187, "Unknown"), (188, "Unknown"),
    (189, "Unknown"), (190, "Unknown"), (191, "Unknown"), (192, "Unknown"),
    (193, "Unknown"), (194, "Unknown"), (195, "Unknown"), (196, "Unknown"),
    (197, "Unknown"), (198, "Unknown"), (199, "Unknown"), (200, "Unknown"),
    (201, "Unknown"), (202, "Unknown"), (203, "Unknown"), (204, "Unknown"),
    (205, "Unknown"), (206, "Unknown"), (207, "Unknown"), (208, "Unknown"),
    (209, "Unknown"), (210, "Unknown"), (211, "Unknown"), (212, "Unknown"),
    (213, "Unknown"), (214, "Unknown"), (215, "Unknown"), (216, "Unknown"),
    (217, "Unknown"), (218, "Unknown"), (219, "Unknown"), (220, "Unknown"),
    (221, "Unknown"), (222, "Unknown"), (223, "Unknown"), (224, "Unknown"),
    (225, "Unknown"), (226, "Unknown"), (227, "Unknown"), (228, "Unknown"),
    (229, "Unknown"), (230, "Unknown"), (231, "Unknown"), (232, "Unknown"),
    (233, "Unknown"), (234, "Unknown"), (235, "Unknown"), (236, "Unknown"),
    (237, "Unknown"), (238, "Unknown"), (239, "Unknown"), (240, "Unknown"),
    (241, "Unknown"), (242, "Unknown"), (243, "Unknown"), (244, "Unknown"),
    (245, "Unknown"), (246, "Unknown"), (247, "Unknown"), (248, "Unknown"),
    (249, "Unknown"), (250, "Unknown"), (251, "Unknown"), (252, "Unknown"),
    (253, "Unknown"), (254, "Unknown"), (255, "My Team"),
];

/// F1 25 tracks (track id -> track name), sorted by id.
#[rustfmt::skip]
pub(super) const TRACKS_2025: &[(i8, &str)] = &[
    (0, "Melbourne"), (1, "Paul Ricard"), (2, "Shanghai"), (3, "Sakhir (Bahrain)"),
    (4, "Catalunya"), (5, "Monaco"), (6, "Montreal"), (7, "Silverstone"),
    (8, "Hockenheim"), (9, "Hungaroring"), (10, "Spa"), (11, "Monza"),
    (12, "Singapore"), (13, "Suzuka"), (14, "Abu Dhabi"), (15, "Texas"),
    (16, "Brazil"), (17, "Austria"), (18, "Sochi"), (19, "Mexico"),
    (20, "Baku (Azerbaijan)"), (21, "Sakhir Short"), (22, "Silverstone Short"), (23, "Texas Short"),
    (24, "Suzuka Short"), (25, "Hanoi"), (26, "Zandvoort"), (27, "Imola"),
    (28, "Portimão"), (29, "Jeddah"), (30, "Miami"), (31, "Las Vegas"),
    (32, "Losail"),
];
