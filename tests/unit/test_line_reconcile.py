"""Tests for ``mokuro_bunko.ocr.line_reconcile``: two reads of a line, merged.

Pure text in, text out -- no model, no image. ``vlm`` below is whatever the
configured engine read (PaddleOCR-VL in production), ``ctc`` the PP-OCRv6 CTC
read of the same line. Every sentence here is made up for the test.
"""

from __future__ import annotations

import pytest

from mokuro_bunko.ocr import line_reconcile as lr


def merged(vlm: str, ctc: str, cells: int = 0, **kwargs: object) -> str:
    return lr.reconcile_line(vlm, ctc, cells, **kwargs).text  # type: ignore[arg-type]


class TestBracketRecovery:
    def test_dropped_opener_and_closer_come_back_from_the_ctc_read(self) -> None:
        line = lr.reconcile_line("今日は雨が降るらしい", "「今日は雨が降るらしい」", 12)
        assert line.text == "「今日は雨が降るらしい」"
        assert line.notes == ["opener", "closer"]
        assert line.source == "merged" and line.agreement == 1.0

    def test_trailing_stop_and_comma_count_as_closers(self) -> None:
        assert merged("傘を忘れてしまった", "傘を忘れてしまった。") == "傘を忘れてしまった。"
        assert merged("傘を忘れて", "傘を忘れて、") == "傘を忘れて、"

    def test_closer_after_the_engines_own_stop(self) -> None:
        assert merged("「もう帰ろう。", "「もう帰ろう。」") == "「もう帰ろう。」"

    def test_the_engines_characters_still_win_inside_the_brackets(self) -> None:
        # the CTC read has the bracket AND a misread kanji; only the bracket is taken
        line = lr.reconcile_line("駅まで歩いて帰ることにした", "「駅まで歩いて掃ることにした」", 15)
        assert line.text == "「駅まで歩いて帰ることにした」"
        assert 0.9 < (line.agreement or 0) < 1.0

    def test_no_mark_without_the_same_glyphs_next_to_it(self) -> None:
        """The glyphs at the mark must align, or nobody knows what the mark opened."""
        line = lr.reconcile_line("犬が庭を走り回っている", "「大が庭を走り回っている", 12)
        assert line.text == "犬が庭を走り回っている" and "opener" not in line.notes

    def test_a_short_line_is_anchored_by_all_of_itself(self) -> None:
        assert merged("はい", "「はい」") == "「はい」"

    def test_marks_inside_the_line_are_not_end_marks(self) -> None:
        assert merged("そうだねと言った", "そうだね」と言った") == "そうだねと言った"


class TestWidth:
    def test_printed_forms_replace_the_engines_folded_ones(self) -> None:
        line = lr.reconcile_line("「まさか!」", "「まさか！」", 5)
        assert line.text == "「まさか！」" and line.notes == ["width"] and line.agreement == 1.0
        assert merged("本当に?", "本当に？") == "本当に？"
        assert merged("12月3日に会おう", "１２月３日に会おう") == "１２月３日に会おう"

    def test_ellipsis_length_comes_from_the_read_that_counts_cells(self) -> None:
        assert merged("...そうか", "……そうか") == "……そうか"
        assert merged("…そうか", "……そうか") == "……そうか"
        assert merged("そうか......", "そうか…") == "そうか…"

    def test_a_pair_in_one_cell_stays_ascii_when_the_ctc_read_has_it_so(self) -> None:
        assert merged("なんだって!?", "なんだって!?") == "なんだって!?"
        assert merged("やめろ!!", "やめろ!!") == "やめろ!!"

    def test_default_policy_where_the_ctc_read_does_not_vouch(self) -> None:
        # the CTC read misses the mark altogether: lone marks widen, pairs stay
        assert merged("待ってくれ!", "待ってくれ") == "待ってくれ！"
        assert lr.widen_punctuation("なに....!?そう...か!") == "なに…!?そう…か！"
        assert lr.widen_punctuation("え..") == "え‥"

    def test_latin_context_is_left_alone(self) -> None:
        assert lr.widen_punctuation("No.1だ") == "No.1だ"
        assert lr.widen_punctuation("OK!") == "OK!"
        assert lr.widen_punctuation("Ver.2!") == "Ver.2!"

    def test_half_width_kana_are_not_a_printed_form_worth_keeping(self) -> None:
        assert merged("アイス", "ｱｲｽ") == "アイス"

    def test_a_character_folding_to_two_is_never_half_replaced(self) -> None:
        assert merged("なに!?", "なに⁉") == "なに!?"


class TestGridCells:
    def test_blank_cell_after_a_mark_and_second_dash_cell(self) -> None:
        line = lr.reconcile_line("「嫌なのか?―すぐに終わるさ」", "「嫌なのか？　――すぐに終わるさ」", 16)
        assert line.text == "「嫌なのか？　――すぐに終わるさ」"
        assert set(line.notes) == {"width", "dash"} and line.agreement == 1.0

    def test_a_long_vowel_mark_that_cannot_be_one_is_a_dash(self) -> None:
        assert merged("何だ?ーまさか", "何だ？　――まさか") == "何だ？　――まさか"

    def test_a_real_long_vowel_is_the_engines_call(self) -> None:
        assert merged("スーパーに行く", "ス―パ―に行く") == "スーパーに行く"

    def test_blank_cells_elsewhere_are_not_adopted(self) -> None:
        assert merged("山と川", "山　と川") == "山と川"
        assert merged("山と川", "山 と川") == "山と川"

    def test_word_gaps_in_latin_and_digits_come_back(self) -> None:
        line = lr.reconcile_line("ONEPUNCH9141", "ONE PUNCH 9 141", 12)
        assert line.text == "ONE PUNCH 9 141" and line.notes == ["space"] and line.agreement == 1.0

    def test_thin_glyph_at_a_line_end_only_in_a_text_body(self) -> None:
        assert merged("廊下がまっすぐ真", "廊下がまっすぐ真一", thin=True) == "廊下がまっすぐ真一"
        assert merged("廊下がまっすぐ真", "廊下がまっすぐ真一") == "廊下がまっすぐ真"
        line = lr.reconcile_line("それは違うと思った", "―それは違うと思った", thin=True)
        assert line.text == "―それは違うと思った" and line.notes == ["thin"]


class TestRunawayAndEmpty:
    def test_runaway_falls_back_to_the_ctc_read_and_says_so(self) -> None:
        line = lr.reconcile_line("ぐくーーーーーーーーーーーーーーーーーー", "ぐくっ", 3)
        assert (line.text, line.source) == ("ぐくっ", "ctc")
        assert "runaway" in line.notes and (line.agreement or 0) < 0.5

    def test_a_repeating_tail_is_a_runaway_even_when_not_much_longer(self) -> None:
        text = "そうだそうだそうだそうだ"
        assert lr.repeated_tail(text) == len(text)
        assert lr.is_runaway("行こうそうだそうだそうだそうだ", "行こうそうだ", 6)

    def test_a_sound_effect_is_not_a_runaway(self) -> None:
        """Repetition alone proves nothing: the CTC read and the quad are as long."""
        line = lr.reconcile_line("ドドドドドドドド", "ドドドドドドドド", 8)
        assert (line.text, line.source, line.agreement) == ("ドドドドドドドド", "merged", 1.0)
        assert not lr.is_runaway("ドドドドドドドド", "", 8)

    def test_a_long_line_that_fits_its_quad_is_not_a_runaway(self) -> None:
        text = "あ" * 40
        assert not lr.is_runaway(text, "あ" * 28, 40)  # the CTC read dropped glyphs

    def test_latin_letters_take_half_a_cell(self) -> None:
        """A horizontal Latin line has twice the characters its quad has ems."""
        line = lr.reconcile_line("ABCDEFGHIJKLMNOPQRSTUVWX", "ABCDEFGHIJ", 10)
        assert (line.text, line.source) == ("ABCDEFGHIJKLMNOPQRSTUVWX", "merged")
        # ... and a runaway in Latin is still one
        line = lr.reconcile_line("OKOKOKOKOKOKOKOKOKOKOKOKOKOKOKOK", "OK", 2)
        assert line.source == "ctc" and "runaway" in line.notes

    def test_runaway_with_no_ctc_read_is_cut_at_the_repetition(self) -> None:
        line = lr.reconcile_line("ガーーーーーーーーーーーー", "", 2)
        assert line.text == "ガ" and line.notes == ["runaway"] and line.engine_only

    def test_a_sound_effect_that_merely_overruns_its_cells_is_left_alone(self) -> None:
        """Display lettering does not keep to the pitch its quad was measured by.

        "ガーーーー" is five glyphs in a quad two cells long -- the trailing
        vowels drawn small inside the first stroke -- and with no CTC read to
        fall back on, cutting it would invent a text nobody read. Only a
        repetition that has filled the quad TWICE over is a runaway here.
        """
        line = lr.reconcile_line("ガーーーー", "", 2)
        assert line.text == "ガーーーー" and line.notes == [] and line.engine_only
        assert not lr.engine_looped("ガーーーー", 2)
        assert lr.engine_looped("ガーーーーーーーーーーーー", 2)
        assert not lr.engine_looped("ガーーーーーーーーーーーー", 0)  # no quad measured
        assert not lr.engine_looped("あいうえおかきくけこさしすせそた", 2)  # long, but no loop

    def test_a_repetition_over_a_region_is_measured_against_one_column(self) -> None:
        """A blob three body pitches thick has room for three columns, so the
        whole-quad bound sits three times too far away: "あい" thirty-one times
        fits inside twice 30 cells and is a runaway from its tenth glyph. The
        repetition itself is measured against the reading axis instead."""
        cells = lr.line_cells(270.0, 81.0, 27.0)
        axis = lr.axis_cells(270.0, 81.0, 27.0)
        assert (cells, axis) == (30, 10)
        assert not lr.engine_looped("あい" * 31, cells)  # the hole: 62 cells against 62
        assert lr.engine_looped("あい" * 31, cells, axis)
        assert not lr.engine_looped("あい" * 4, cells, axis)  # lettering that repeats
        # ... and a read that merely ENDS in a repetition is judged on the
        # whole quad, where a region has all its room
        assert not lr.engine_looped("あ" * 20 + "いう" * 4, cells, axis)

    def test_empty_engine_output_uses_the_ctc_read(self) -> None:
        line = lr.reconcile_line("  ", "「はい」", 4)
        assert (line.text, line.source, line.notes, line.agreement) == (
            "「はい」", "ctc", ["empty"], None,
        )  # fmt: skip
        nothing = lr.reconcile_line("", "", 0)
        assert (nothing.text, nothing.notes) == ("", [])

    def test_token_budget_follows_the_quads_cells(self) -> None:
        assert lr.line_cells(2500.0, 62.5) == 40
        assert lr.line_cells(0.0, 60.0) == 0
        assert lr.token_cap(40) == 68
        assert lr.token_cap(1) == lr.TOKENS_FLOOR
        assert lr.token_cap(10_000) == lr.TOKENS_CEILING
        # a runaway is cut by the cap well before the fallback has to catch it
        assert lr.token_cap(3) < 20


class TestQuadRoom:
    """A quad that is no line is measured as a region (bench: Saki 02 page 129).

    The detector returns the page's dense hand-lettered panel as ONE near-square
    blob over ten slanted columns (196x159 px on a page whose body pitch is 27),
    and its two-row afterword header as one box.
    """

    def test_a_line_is_its_length_in_thicknesses(self) -> None:
        assert lr.line_cells(270.0, 27.0) == 10
        assert lr.line_cells(270.0, 27.0, 27.0) == 10  # a pitch changes nothing here
        assert lr.line_cells(46.0, 27.0, 27.0) == 2

    def test_a_blob_over_several_columns_has_room_for_all_of_them(self) -> None:
        assert lr.line_cells(196.0, 159.0) == 1  # ... the aspect ratio of a square
        assert lr.is_region(196.0, 159.0, 27.0)
        assert lr.line_cells(196.0, 159.0, 27.0) == 42  # 7 columns of 6 cells
        assert lr.token_cap(42) > 60  # ... enough budget to read them

    def test_the_reading_axis_of_a_blob_is_one_of_its_columns(self) -> None:
        """How far ONE run of glyphs can go, which is not the room the blob has."""
        assert lr.axis_cells(270.0, 27.0, 27.0) == 10  # a line: its own cells
        assert lr.axis_cells(270.0, 27.0) == 10  # ... with no pitch as well
        assert lr.axis_cells(196.0, 159.0, 27.0) == 7  # a region: one column of it
        assert lr.line_cells(196.0, 159.0, 27.0) == 42  # ... of the 42 cells it holds
        assert lr.axis_cells(0.0, 27.0, 27.0) == 0

    def test_display_lettering_is_a_line_and_not_a_region(self) -> None:
        """Bigger than the body, but nowhere near the thickness of a panel."""
        assert not lr.is_region(124.0, 47.0, 27.0)
        assert lr.line_cells(124.0, 47.0, 27.0) == 3

    def test_without_a_pitch_nothing_is_a_region(self) -> None:
        assert not lr.is_region(196.0, 159.0, 0.0)
        assert lr.line_cells(196.0, 159.0, 0.0) == 1
        assert lr.region_cells(196.0, 159.0, 0.0) == 0

    def test_a_region_read_is_no_runaway_and_a_repetition_still_is(self) -> None:
        text = "あいうえおかきくけこさしすせそた"
        assert lr.is_runaway(text, "", lr.line_cells(196.0, 159.0))  # the bug: room for 1
        assert not lr.is_runaway(text, "", lr.line_cells(196.0, 159.0, 27.0))
        assert lr.is_runaway("あ" * 90, "", lr.line_cells(196.0, 159.0, 27.0))

    def test_the_header_the_ctc_read_only_a_third_of_keeps_its_engine_read(self) -> None:
        """Page 129's "あとがきおまけまんが": read as "まんが" by the CTC recognizer.

        Two rows in one box, so the box is as thick as it is long and the line
        formula gives it one cell; the engine's ten glyphs were a runaway
        against that and the line fell back to the three the CTC read had.
        """
        cells = lr.line_cells(136.0, 108.0, 27.0)
        line = lr.reconcile_line("あいうえおかきくけこ", "くけこ", cells)
        assert line.text == "あいうえおかきくけこ" and "runaway" not in line.notes
        was = lr.reconcile_line("あいうえおかきくけこ", "くけこ", lr.line_cells(136.0, 108.0))
        assert (was.text, was.source) == ("くけこ", "ctc") and "runaway" in was.notes

    def test_a_long_read_against_a_short_one_stands_only_where_the_quad_has_room(self) -> None:
        """Page 129 line 19: 13 glyphs of handwriting, 6 of them read by the CTC."""
        long_read, short_read = "あいうえおかきくけこさしす", "おかきくけ」"
        assert merged(long_read, short_read, lr.line_cells(125.0, 64.0, 27.0)) == long_read
        # the same two reads on a quad with room for six glyphs: the CTC read
        assert merged(long_read, short_read, lr.line_cells(125.0, 21.0, 27.0)) == short_read


class TestDisagreement:
    def test_a_different_read_is_never_patched(self) -> None:
        line = lr.reconcile_line("全然ちがう文章です", "「あいうえおかきく」", 10)
        assert line.text == "全然ちがう文章です"
        assert line.notes == ["disagree"] and (line.agreement or 0) < 0.3

    def test_agreement_is_what_is_left_after_the_structural_patches(self) -> None:
        same = lr.reconcile_line("駅前で待つ!", "「駅前で待つ！」", 7)
        assert same.agreement == 1.0  # brackets and width are not disagreement
        kanji = lr.reconcile_line("息が詰まりそうな街だった。", "鳥が詰まりそうな街だった。", 13)
        assert kanji.text.startswith("息") and kanji.agreement == pytest.approx(12 / 13)

    def test_json_for_the_raw_dump(self) -> None:
        entry = lr.reconcile_line("駅前で待つ!", "「駅前で待つ！」", 7).to_json()
        assert entry == {
            "vlm": "駅前で待つ!",
            "ctc": "「駅前で待つ！」",
            "merged": "「駅前で待つ！」",
            "agreement": 1.0,
            "source": "merged",
            "notes": ["opener", "width", "closer"],
        }

    def test_page_summary(self) -> None:
        lines = [
            lr.reconcile_line("駅前で待つ!", "「駅前で待つ！」", 7),
            lr.reconcile_line("息が詰まりそうな街だった。", "鳥が詰まりそうな街だった。", 13),
            lr.reconcile_line("", "はい", 2),
        ]
        summary = lr.page_summary(lines)
        assert (summary["lines"], summary["compared"], summary["full_agreement"]) == (3, 2, 1)
        assert summary["from_ctc"] == 1 and summary["notes"]["empty"] == 1


class TestShortLines:
    def test_a_confident_ctc_kanji_stands_on_a_line_without_context(self) -> None:
        line = lr.reconcile_line("エち", "巧", 1, ctc_conf=0.99)
        assert (line.text, line.source) == ("巧", "ctc") and "short" in line.notes
        assert not lr.needs_second_read(line)

    def test_a_confident_glyph_beats_a_bare_mark(self) -> None:
        assert merged("!", "の", 1, ctc_conf=0.99) == "の"
        assert merged("!", "の", 1, ctc_conf=0.6) == "!"  # a doubting CTC read proves nothing

    def test_a_doubting_ctc_read_or_a_kana_line_is_the_engines(self) -> None:
        assert merged("目", "日", 1, ctc_conf=0.68) == "目"
        assert merged("ザッ", "ガッ", 2, ctc_conf=0.95) == "ザッ"

    def test_longer_lines_are_untouched_by_the_rule(self) -> None:
        assert merged("息が詰まる", "鳥が詰まる", 5, ctc_conf=0.99) == "息が詰まる"


class TestSecondRead:
    def test_two_reads_out_of_three_settle_a_kanji(self) -> None:
        first = lr.reconcile_line("両膝で馬の胴を抜んで締める。", "両滕で馬の胴を挟んで締める。", 14)
        assert lr.needs_second_read(first)
        settled = lr.settle_disputes(first, "両膝で馬の胴を挟んで締める。", 14)
        assert settled.text == "両膝で馬の胴を挟んで締める。"  # 挟 voted in, 膝 kept
        assert "vote" in settled.notes and settled.second is not None
        assert settled.agreement == pytest.approx(13 / 14)

    def test_a_three_way_split_changes_nothing(self) -> None:
        first = lr.reconcile_line("毛を毟るように掴む", "毛を笔るように掴む", 9)
        settled = lr.settle_disputes(first, "毛を筆るように掴む", 9)
        assert settled.text == "毛を毟るように掴む" and "vote" not in settled.notes

    def test_glyphs_the_first_read_dropped_come_back(self) -> None:
        first = lr.reconcile_line("灯りがともった。", "灯りがともった。遠く", 10)
        assert lr.settle_disputes(first, "灯りがともった。遠く", 10).text == "灯りがともった。遠く"

    def test_the_missing_glyph_mark_never_wins(self) -> None:
        first = lr.reconcile_line("音が闇に谺して", "音が闇に〓して", 7)
        assert lr.settle_disputes(first, "音が闇に谺して", 7).text == "音が闇に谺して"

    def test_neighbouring_disputes_are_voted_on_glyph_by_glyph(self) -> None:
        """Two differing glyphs side by side are ONE stretch to the aligner.

        Swapped as a whole, the second read's vote for the right-hand glyph
        would carry the CTC read's missing-glyph mark in with it.
        """
        first = lr.reconcile_line("あい甲乙うえお", "あい〓丙うえお", 7)
        settled = lr.settle_disputes(first, "あい丁丙うえお", 7)
        assert settled.text == "あい甲丙うえお" and "vote" in settled.notes
        # the same with two real glyphs: each is settled on its own evidence
        first = lr.reconcile_line("あい甲乙うえお", "あい丁丙うえお", 7)
        assert lr.settle_disputes(first, "あい甲丙うえお", 7).text == "あい甲丙うえお"

    def test_a_missing_glyph_mark_is_never_voted_in_with_its_stretch(self) -> None:
        first = lr.reconcile_line("あい甲うえお", "あい〓乙うえお", 7)
        settled = lr.settle_disputes(first, "あい乙うえお", 7)
        assert "〓" not in settled.text

    def test_a_line_end_dash_the_second_read_wrote_its_own_way_is_a_vote(self) -> None:
        """The engine's "ー" after a kanji is the CTC read's "―" (bench page 283)."""
        first = lr.reconcile_line("甲乙丙の事件簿", "甲乙丙の事件薄―", 8)
        assert first.text == "甲乙丙の事件簿"  # not a text body: no thin-glyph restore
        settled = lr.settle_disputes(first, "甲乙丙の事件簿ー", 8)
        assert settled.text == "甲乙丙の事件簿―" and "vote" in settled.notes
        # after a kana "ー" is a long vowel, and no witness for a dash
        first = lr.reconcile_line("あいうえおか", "あいうえおか―", 7)
        assert lr.settle_disputes(first, "あいうえおかー", 7).text == "あいうえおか"

    def test_full_agreement_needs_no_second_read(self) -> None:
        assert not lr.needs_second_read(lr.reconcile_line("はい!", "はい！", 3))

    def test_an_unusable_first_read_is_replaced_by_a_usable_second(self) -> None:
        first = lr.reconcile_line("ぐくーーーーーーーーーーーーーー", "ぐくっ!", 4)
        assert first.source == "ctc" and lr.needs_second_read(first)
        retried = lr.settle_disputes(first, "ぐくっ!", 4)
        assert (retried.text, retried.source) == ("ぐくっ!", "merged")
        assert retried.notes[0] == "runaway" and retried.notes[-1] == "retry"

    def test_a_second_read_of_an_engine_only_line_is_recorded_not_believed(self) -> None:
        """It can only say "same again", and it nearly always does; see the verdict."""
        first = lr.reconcile_line("ザッ", "", 2)
        assert first.engine_only and first.agreement is None and lr.needs_second_read(first)
        assert lr.settle_disputes(first, "ザッ", 2).confirmed
        other = lr.settle_disputes(lr.reconcile_line("ドド", "", 2), "ドドド", 2)
        assert not other.confirmed and other.text == "ドド"
        doubted = lr.reconcile_line("ゴゴゴゴ", "何コH", 4, ctc_conf=0.42)
        assert doubted.engine_only and doubted.to_json()["engine_only"] is True
        trusted = lr.reconcile_line("ゴゴゴゴ", "何コH", 4, ctc_conf=0.8)
        assert not trusted.engine_only


class TestEngineOnlyVerdict:
    """Lettering or line art? Decided on the DETECTOR's score for the quad.

    Measured against 335 engine-only lines judged by eye across six volumes,
    split into page-disjoint halves. Only the ``body`` parameters were swept,
    on one half and reported on the other; the two detector bars were not
    swept at all (``engine_only_verdict`` says where each of them came from).
    What that corpus says, and what these tests hold the rule to:

    * the detector's score separates (0.70-0.80 is 58% lettering, 0.80-0.90 is
      81% -- 56 of 69 -- and over 0.90 is 100%); nothing else in the guard
      used it;
    * in that middle band the quad's COMPANY separates what the score cannot:
      a small glyph with read lines parallel to it nearby is printed ruby or
      small kana 13 times in 14 (the ``body`` rule);
    * a second engine read does not separate at all -- it repeats the first
      read on 311 of the 335 lines, phantom or not;
    * display size separates weakly and is where every false keep came in;
    * a quad thinner than the body pitch is NOT art by that alone: the 0.50 to
      0.75 pitch band is more lettering than not (a printed "し" or "と" gets a
      narrow box), so the old thinness test is gone.

    Geometry here is the placeholder page of the other classes: body pitch 27.
    """

    PITCH = 27.0

    def verdict(
        self,
        vlm: str,
        det: float,
        main: float,
        thick: float,
        second: str | None = None,
        neighbours: int = 0,
    ) -> tuple[bool, str]:
        cells = lr.line_cells(main, thick, self.PITCH)
        line = lr.settle_disputes(lr.reconcile_line(vlm, "", cells), vlm if second is None else second, cells)  # fmt: skip
        return lr.engine_only_verdict(
            line,
            cells=cells,
            det_score=det,
            main=main,
            thickness=thick,
            pitch=self.PITCH,
            neighbours=neighbours,
        )

    def test_a_quad_the_detector_is_sure_of_is_kept(self) -> None:
        """Body-size hand-lettering: what the display test threw away."""
        assert self.verdict("ぐり", 0.86, 46.0, 27.0) == (True, "backed")

    def test_a_thin_quad_is_no_longer_art_by_its_thickness(self) -> None:
        """A printed "し" gets a box half the body pitch wide. So does a hair
        stroke; the detector, not the box, is what tells them apart."""
        assert self.verdict("し", 0.88, 30.0, 14.0) == (True, "backed")
        assert self.verdict("し", 0.44, 30.0, 14.0) == (False, "unbacked")

    def test_a_quad_the_detector_doubts_is_dropped_however_big_the_lettering(self) -> None:
        # display-size, read the same way twice, and still art four times in ten
        assert self.verdict("ハッ", 0.55, 120.0, 60.0) == (False, "unbacked")
        assert self.verdict("ハッ", 0.79, 120.0, 60.0) == (False, "unbacked")
        assert self.verdict("ハッ", 0.81, 120.0, 60.0) == (True, "backed")

    def test_a_hand_lettered_panel_is_kept_under_that_bar(self) -> None:
        """One quad over ten slanted columns: ink the detector is right about
        and unsure of at once. A phrase must have come out of it."""
        read = "あいうえおかきくけこさしすせそた"
        assert self.verdict(read, 0.66, 196.0, 159.0) == (True, "region")
        assert self.verdict(read, 0.44, 196.0, 159.0) == (False, "unbacked")

    def test_one_glyph_over_a_whole_panel_is_hatching(self) -> None:
        assert self.verdict("ギ", 0.66, 196.0, 159.0) == (False, "unbacked")
        # ... but the same blob with a sure detector is kept, and that is one
        # of the six false keeps this rule accepts on held-out data.
        assert self.verdict("ギ", 0.86, 196.0, 159.0) == (True, "backed")

    def test_a_second_read_decides_nothing_either_way(self) -> None:
        assert self.verdict("ドド", 0.86, 120.0, 60.0, second="ドドド") == (True, "backed")
        assert self.verdict("ザッ", 0.50, 120.0, 60.0, second="ザッ") == (False, "unbacked")

    def test_a_loop_is_dropped_whatever_the_detector_says(self) -> None:
        """The runaway protection, kept for its one real case and measured
        against the quad's cells -- there is no CTC read here to measure."""
        assert self.verdict("ガーーーーーーーーーーーー", 0.95, 60.0, 27.0) == (False, "looped")
        assert self.verdict("あ" * 40, 0.95, 270.0, 27.0) == (False, "looped")
        assert self.verdict("ガーーーー", 0.95, 60.0, 27.0) == (True, "backed")

    def test_a_region_that_repeats_one_unit_is_a_runaway_at_the_lower_bar(self) -> None:
        """The hole the region rule opened: a blob three pitches thick has room
        for three columns of glyphs, so the whole-quad bound is three times as
        far away -- over exactly the quads ``DETECTOR_REGION`` admits at 0.60.
        A read that is nothing but one repeated unit is measured against ONE
        column instead (:func:`lr.axis_cells`)."""
        blob = ("あい" * 31, 0.65, 270.0, 81.0)  # 62 glyphs over 10 cells x 3 rows
        assert lr.line_cells(270.0, 81.0, self.PITCH) == 30  # ... the old budget: 62
        assert lr.axis_cells(270.0, 81.0, self.PITCH) == 10  # ... one column of it
        assert self.verdict(*blob) == (False, "looped")
        # ... and lettering that genuinely repeats over a panel is not a loop
        assert self.verdict("あいあいあいあい", 0.65, 405.0, 81.0) == (True, "region")

    def test_a_small_glyph_among_read_lines_is_kept_under_the_bar(self) -> None:
        """Printed ruby, a printed kana alone in its column, the small "ッ" of a
        sound effect: half a body cell thick, with lines the CTC recognizer
        read running parallel to them. That company is what art has not."""
        ruby = (19.0, 40.0)  # 0.7 of the body pitch, two cells long
        assert self.verdict("あい", 0.76, ruby[1], ruby[0], neighbours=1) == (True, "body")
        assert self.verdict("あい", 0.76, ruby[1], ruby[0]) == (False, "unbacked")
        # ... and the bar is not gone, only lower
        assert self.verdict("あい", 0.62, ruby[1], ruby[0], neighbours=1) == (False, "unbacked")

    def test_the_body_rule_asks_for_a_glyph_smaller_than_the_body(self) -> None:
        """A quad as thick as the body cell, or thicker, is not ruby and not
        small kana; beside a column of text it is as likely to be hatching."""
        assert self.verdict("あい", 0.76, 40.0, 24.0, neighbours=2) == (False, "unbacked")
        assert self.verdict("あい", 0.76, 40.0, 60.0, neighbours=2) == (False, "unbacked")
        assert self.verdict("あい", 0.76, 40.0, 21.0, neighbours=2) == (True, "body")

    def test_without_a_pitch_only_the_detector_speaks(self) -> None:
        """A caller with no page to measure: no region, no body, no thinness test."""
        line = lr.settle_disputes(lr.reconcile_line("ドキ", "", 1), "ドキ", 1)
        assert lr.engine_only_verdict(line, cells=1, det_score=0.86, thickness=9.3) == (
            True, "backed",
        )  # fmt: skip
        assert lr.engine_only_verdict(line, cells=1, det_score=0.70, thickness=9.3) == (
            False, "unbacked",
        )  # fmt: skip
        assert lr.engine_only_verdict(
            line, cells=1, det_score=0.70, main=40.0, thickness=9.3, neighbours=3
        ) == (False, "unbacked")

    def test_over_a_region_the_wider_crop_may_read_on_past_the_narrower(self) -> None:
        """Two crops of a panel do not see the same thing; of a line they do."""
        narrow, wide = "あいうえおかきくけこ", "あいうえおかきくけこさしすせそ"
        assert lr.corroborates(narrow, wide, cells=42)
        assert not lr.corroborates(narrow, wide, cells=2)
        assert not lr.corroborates(narrow, "たちつてとなにぬねの", cells=42)
        assert not lr.corroborates(narrow, "", cells=42)
        # ... and two glyphs inside three are a second opinion, room or no room
        assert not lr.corroborates("ドド", "ドドド", cells=42)

    def test_the_rule_that_decided_is_in_the_dump(self) -> None:
        cells = lr.line_cells(196.0, 159.0, 27.0)
        read = "あいうえおかきくけこさしすせそた"
        line = lr.settle_disputes(lr.reconcile_line(read, "", cells), read, cells)
        keep, why = lr.engine_only_verdict(
            line, cells=cells, det_score=0.66, main=196.0, thickness=159.0, pitch=27.0
        )
        line.notes.append(why)
        entry = line.to_json()
        assert keep and entry["notes"] == ["confirmed", "region"]
        assert entry == {**entry, "engine_only": True, "confirmed": True, "merged": read}
        dropped = lr.settle_disputes(lr.reconcile_line("ギ", "", cells), "ギ", cells)
        keep, why = lr.engine_only_verdict(
            dropped, cells=cells, det_score=0.66, main=196.0, thickness=159.0, pitch=27.0
        )
        dropped.notes.extend([why, "dropped"])
        # "confirmed" is still recorded, and is still no reason to keep it
        assert not keep and dropped.to_json()["notes"] == ["confirmed", "unbacked", "dropped"]


class TestOverlapRepeat:
    def test_the_seam_two_crops_share_is_found(self) -> None:
        assert lr.overlap_repeat("柱、あざやかな飾り、なのに", "飾り、なのにど", 6) == 6
        assert lr.overlap_repeat("あいうえお", "かきくけこ", 5) == 0

    def test_never_more_than_the_shared_stretch_has_room_for(self) -> None:
        """Two halves of a sound effect share no ink, whatever they read."""
        assert lr.overlap_repeat("ドドド", "ドドド", 0) == 0
        assert lr.overlap_repeat("ドドド", "ドドド", 1) == 1


class TestWholeBookRegressions:
    """What a 292-page novel showed and 24 bench pages had not (placeholder wording)."""

    def test_a_run_of_dashes_is_not_a_long_vowel(self) -> None:
        # after a kana the engine's "ー" could be a long vowel -- but not where
        # the read that counts cells has TWO dash cells
        assert merged("あいにーかきくけこーさしすせそ", "あいに――かきくけこ――さしすせそ", 17) == (
            "あいに――かきくけこ――さしすせそ"
        )
        # a line that opens with a dash run is still the same line, and keeps its length
        assert merged("ーあいうえお。", "―――あいうえお。", 9) == "―――あいうえお。"
        # the closer behind the run comes back as well
        assert merged("「あいうえおかきくけこー", "「あいうえおかきくけこ――」", 14) == (
            "「あいうえおかきくけこ――」"
        )
        # one dash cell against a possible long vowel stays the engine's call
        assert merged("あいうえおかー", "あいうえおか―", 7) == "あいうえおかー"

    def test_the_last_glyphs_of_a_wrapped_sentence_keep_their_bracket(self) -> None:
        """ "X」" alone in its column: the engine reads the bracket as く, レ or ー."""
        for engine in ("あく", "あー", "あレ", "ぁ"):
            line = lr.reconcile_line(engine, "あ」", 2, ctc_conf=0.98)
            assert (line.text, line.source) == ("あ」", "ctc") and "short" in line.notes
        assert merged("あく", "あ」", 2, ctc_conf=0.7) == "あく"  # a doubting CTC read proves nothing
        assert merged("あ」", "あ」", 2, ctc_conf=0.98) == "あ」"

    def test_glyphs_the_engine_skipped_come_back_when_the_ctc_read_is_sure_of_them(self) -> None:
        # the dangling start of the next sentence, after a line's last stop
        ctc = "あいうえおかきくけこ。漢"
        sure = [0.99] * len(ctc)
        line = lr.reconcile_line("あいうえおかきくけこ。", ctc, 12, ctc_char_confs=sure)
        assert line.text == ctc and "skipped" in line.notes and line.agreement == 1.0
        # a first sentence (with its blank cell) and a stretch in mid-line
        ctc = "「あいう？　えおかきくけこさし」"
        line = lr.reconcile_line("えおかきくけこさし", ctc, 16, ctc_char_confs=[0.99] * len(ctc))
        assert line.text == ctc
        ctc = "あいうえおこと漢字のことかきくけこ"
        line = lr.reconcile_line("あいうえおことかきくけこ", ctc, 17, ctc_char_confs=[0.99] * len(ctc))
        assert line.text == ctc
        # not from a glyph the CTC read doubts, not a thing that is no text, not unasked
        doubted = [*[0.99] * 11, 0.8]
        assert merged("あいうえおかきくけこ。", "あいうえおかきくけこ。漢", 12, ctc_char_confs=doubted) == (
            "あいうえおかきくけこ。"
        )
        assert merged("あいうえお", "●あいうえお", 6, ctc_char_confs=[0.99] * 6) == "あいうえお"
        assert merged("あいうえおかきくけこ。", "あいうえおかきくけこ。漢", 12) == "あいうえおかきくけこ。"

    def test_kana_the_ctc_read_is_certain_of_stand_against_a_fluent_guess(self) -> None:
        """The engine writes the likelier word: ため for a printed たび, both times."""
        ctc = "あいうするたびにかきくけこ"
        confs = [0.999] * len(ctc)
        line = lr.reconcile_line("あいうするためにかきくけこ", ctc, 13, ctc_char_confs=confs)
        assert line.text == ctc and "kana" in line.notes
        # an ordinary confidence leaves the engine's kana alone (ラ read as う at 0.97)
        confs[6] = 0.97
        assert merged("あいうするためにかきくけこ", ctc, 13, ctc_char_confs=confs) == (
            "あいうするためにかきくけこ"
        )
        # kanji are the engine's whatever the CTC read's confidence
        ctc = "あいう甲にかきくけこ"
        assert merged("あいう乙にかきくけこ", ctc, 10, ctc_char_confs=[0.999] * 10) == (
            "あいう乙にかきくけこ"
        )

    def test_an_ellipsis_the_ctc_read_spelled_in_dots_still_gives_its_length(self) -> None:
        assert merged("「…」", "「.........」", 5) == "「………」"
        assert merged("「…あいうえお", "「.………あいうえお", 9) == "「………あいうえお"
        assert merged("「…あいうえお", "「……あいうえお", 8) == "「……あいうえお"

    def test_a_second_read_that_has_the_missing_sentence_brings_its_blank_cell(self) -> None:
        first = lr.reconcile_line("えおかきくけこさし", "「あいう？　えおかきくけこさし」", 16)
        settled = lr.settle_disputes(first, "「あいう?えおかきくけこさし」", 16)
        assert settled.text == "「あいう？　えおかきくけこさし」"
