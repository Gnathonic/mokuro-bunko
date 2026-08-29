"""Parity with the reader client's pure helpers.

Every expected value here was produced by running the client's own
implementation under Node (`src/lib/util/count-chars.ts`,
`src/lib/util/series-extraction.ts` `generateDeterministicUUID`,
`src/lib/metadata/series-key.ts`, `src/lib/metadata/sanitize.ts`).
"""

from __future__ import annotations

from mokuro_bunko.metadata.reader_compat import (
    count_chars,
    count_matched_pages,
    count_page_chars,
    deterministic_uuid,
    is_image_extension,
    is_system_file,
    natural_sort_key,
    normalize_series_key,
    normalize_updated_at,
    normalize_volume_title_key,
    trailing_extension,
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

    def test_non_decimal_digits_do_not_explode(self) -> None:
        # `str.isdigit()` is True for 128 codepoints `int()` refuses — circled
        # numerals (①), superscripts (¹), parenthesised forms (⑴). Circled
        # numerals are real Japanese volume numbering, and one such filename
        # must never abort publishing for the whole library.
        assert natural_sort_key("①")
        assert natural_sort_key("10①")

    def test_sorts_a_mix_of_decimal_and_non_decimal_digits(self) -> None:
        titles = ["Vol 10", "Vol 2", "Vol 1①"]
        assert sorted(titles, key=natural_sort_key) == ["Vol 1①", "Vol 2", "Vol 10"]


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


class TestCountMatchedPages:
    """Each case mirrors one of the client's own `matchImagesToPages` tests
    (`src/lib/import/__tests__/processing.test.ts`), reduced to the count."""

    def test_exact_matches(self) -> None:
        assert count_matched_pages(
            ["page001.jpg", "page002.jpg"], ["page001.jpg", "page002.jpg"]
        ) == 2

    def test_detects_missing_images(self) -> None:
        assert count_matched_pages(
            ["page001.jpg", "page002.jpg", "page003.jpg"], ["page001.jpg", "page002.jpg"]
        ) == 2

    def test_extra_images_do_not_inflate_the_count(self) -> None:
        assert count_matched_pages(["page001.jpg"], ["page001.jpg", "bonus.jpg"]) == 1

    def test_remaps_across_a_changed_extension(self) -> None:
        assert count_matched_pages(
            ["page001.png", "page002.png"], ["page001.webp", "page002.webp"]
        ) == 2

    def test_nested_paths(self) -> None:
        assert count_matched_pages(
            ["images/page001.jpg", "images/page002.jpg"],
            ["images/page001.jpg", "images/page002.jpg"],
        ) == 2

    def test_case_insensitive(self) -> None:
        assert count_matched_pages(["PAGE001.JPG"], ["page001.jpg"]) == 1

    def test_backslashes_normalize_to_forward_slashes(self) -> None:
        assert count_matched_pages(["images\\page001.jpg"], ["images/page001.jpg"]) == 1

    def test_count_based_fallback_when_every_name_was_rewritten(self) -> None:
        assert count_matched_pages(
            ["001.png", "002.png", "003.png"],
            ["001_result.webp", "002_result.webp", "003_result.webp"],
        ) == 3

    def test_no_fallback_when_most_names_already_match(self) -> None:
        assert count_matched_pages(
            ["page001.jpg", "page002.jpg", "page003.jpg", "page004.jpg"],
            ["page001.jpg", "page002.jpg", "renamed.jpg", "page004.jpg"],
        ) == 3

    def test_no_fallback_when_the_counts_disagree(self) -> None:
        assert count_matched_pages(
            ["001.png", "002.png", "003.png"], ["001_result.webp", "002_result.webp"]
        ) == 0

    def test_one_file_cannot_back_two_pages_by_stem(self) -> None:
        # `a.png` and `a.jpg` both stem to `a`; only the first claims the file.
        assert count_matched_pages(["a.png", "a.jpg"], ["a.webp"]) == 1

    def test_but_an_exact_match_is_never_consumed(self) -> None:
        # The client's exact branch does not consult `usedFiles`, so two pages
        # naming the same image both match.
        assert count_matched_pages(["a.webp", "a.webp"], ["a.webp"]) == 2

    def test_a_page_without_an_img_path_is_unmatched(self) -> None:
        assert count_matched_pages(["a.jpg", None], ["a.jpg", "b.jpg"]) == 1

    def test_no_pages_is_no_matches_and_no_division_by_zero(self) -> None:
        assert count_matched_pages([], ["a.jpg"]) == 0

    def test_duplicate_archive_entries_count_once(self) -> None:
        # A Map keyed by path on the client side; one file either way.
        assert count_matched_pages(["a.jpg", "b.jpg"], ["a.jpg", "a.jpg"]) == 1


class TestArchiveEntryFilters:
    def test_system_junk(self) -> None:
        assert is_system_file("__MACOSX/._page001.jpg")
        assert is_system_file("._page001.jpg")
        assert is_system_file("Thumbs.db")
        assert is_system_file("page001.jpg~")
        assert is_system_file("page001.jpg.bak")
        assert not is_system_file("images/page001.jpg")

    def test_image_extensions_include_the_modern_pair(self) -> None:
        assert is_image_extension("JPG")
        assert is_image_extension("avif")
        assert is_image_extension("jxl")
        assert not is_image_extension("txt")

    def test_extension_comes_off_the_whole_path_like_the_client(self) -> None:
        assert trailing_extension("images/page001.JPG") == "jpg"
        # The client's quirk: a dot in a directory name swallows the rest.
        assert trailing_extension("chapter.1/page001") == "1/page001"
        assert trailing_extension("page001") == "page001"
