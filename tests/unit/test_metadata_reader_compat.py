"""Parity with the reader client's pure helpers.

Every expected value here was produced by running the client's own
implementation under Node (`src/lib/util/count-chars.ts`,
`src/lib/util/series-extraction.ts` `generateDeterministicUUID`,
`src/lib/metadata/series-key.ts`, `src/lib/metadata/sanitize.ts`).
"""

from __future__ import annotations

from mokuro_bunko.metadata.reader_compat import (
    count_chars,
    count_page_chars,
    deterministic_uuid,
    natural_sort_key,
    normalize_series_key,
    normalize_updated_at,
    normalize_volume_title_key,
)


class TestCountChars:
    def test_matches_the_client_on_real_ocr_lines(self) -> None:
        assert count_chars("あたしはもうちょっとしたＣＯＮＡには") == 14
        assert count_chars("Hello, 世界!") == 2
        assert count_chars("カタカナとひらがな") == 9
        assert count_chars("123 ABC") == 0

    def test_counts_the_explicit_singles_the_client_lists(self) -> None:
        # ○ U+25CB, ◯ U+25EF, 々 U+3005 — and 〆 U+3006 rides along inside the
        # class's `々-〇` range, which is a quirk of the client worth pinning.
        assert count_chars("○◯々") == 3
        assert count_chars("〆") == 1
        assert count_chars("〜①") == 0

    def test_counts_halfwidth_katakana_and_cjk_extension_b(self) -> None:
        assert count_chars("ｱｲｳ") == 3
        assert count_chars("\U00020000") == 1

    def test_page_totals_sum_every_line_of_every_block(self) -> None:
        pages = [
            {"blocks": [{"lines": ["世界", "abc"]}, {"lines": ["ねこ"]}]},
            {"blocks": [{"lines": ["犬"]}]},
        ]
        assert count_page_chars(pages) == 5

    def test_malformed_pages_are_skipped_not_fatal(self) -> None:
        pages = [{"blocks": "nonsense"}, {"blocks": [{"lines": [1, "犬"]}]}, "junk"]
        assert count_page_chars(pages) == 1


class TestDeterministicUUID:
    def test_matches_the_client_for_placeholder_uuids(self) -> None:
        assert deterministic_uuid("Dr Stone/Volume 01") == "38d6c0d6-1bef-4134-a339-a1e254c6"
        assert deterministic_uuid("Bakemonogatari/v01") == "fd95c3db-a308-4539-9e9d-26e2a09e"
        assert deterministic_uuid("Series/Vol 1") == "964edad5-740a-4337-a244-29e20a59"

    def test_lowercases_and_trims_like_the_client(self) -> None:
        assert deterministic_uuid("  MiXeD Case / Vol 2  ") == "7f27f644-c177-46a6-be50-b0e2409f"


class TestKeys:
    def test_series_key_folds_case_and_whitespace(self) -> None:
        assert normalize_series_key("  Dr   STONE  ") == "dr stone"

    def test_volume_title_key_also_folds_unicode_composition(self) -> None:
        assert normalize_volume_title_key("Bände 1") == normalize_volume_title_key("Bände 1")


class TestNaturalSort:
    def test_orders_like_the_clients_numeric_collator(self) -> None:
        titles = ["Vol 10", "Vol 2", "vol 1", "Volume 3", "Extra"]
        assert sorted(titles, key=natural_sort_key) == [
            "Extra",
            "vol 1",
            "Vol 2",
            "Vol 10",
            "Volume 3",
        ]

    def test_is_total_and_stable_for_equal_keys(self) -> None:
        assert natural_sort_key("VOL 1") == natural_sort_key("vol 1")


class TestNormalizeUpdatedAt:
    def test_normalises_to_iso_with_milliseconds(self) -> None:
        assert normalize_updated_at("2026-08-18T19:36:24.324Z") == "2026-08-18T19:36:24.324Z"
        assert normalize_updated_at("2026-08-18T19:36:24Z") == "2026-08-18T19:36:24.000Z"

    def test_rejects_junk(self) -> None:
        assert normalize_updated_at("Aug 16 2020") is None
        assert normalize_updated_at(None) is None
        assert normalize_updated_at(1234) is None

    def test_clamps_the_far_future_to_now(self) -> None:
        now = 1_800_000_000.0  # 2027-01-15T08:00:00Z
        clamped = normalize_updated_at("2999-01-01T00:00:00.000Z", now=now)
        assert clamped == "2027-01-15T08:00:00.000Z"

    def test_tolerates_small_clock_skew(self) -> None:
        now = 1_800_000_000.0
        just_ahead = normalize_updated_at("2027-01-15T08:01:00.000Z", now=now)
        assert just_ahead == "2027-01-15T08:01:00.000Z"
