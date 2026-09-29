"""The OCR queue's one ordering rule: the generations in the operator's own
list order, then round-robin across series with each series in natural
reading order."""

from __future__ import annotations

import pytest

from mokuro_bunko.ocr.job_order import natural_key, order_jobs


def _sorted(names: list[str]) -> list[str]:
    return sorted(names, key=natural_key)


class TestNaturalKey:
    def test_digit_runs_compare_as_numbers(self) -> None:
        assert _sorted(["Volume 10", "Volume 2", "Volume 1"]) == [
            "Volume 1",
            "Volume 2",
            "Volume 10",
        ]

    def test_zero_padding_does_not_matter(self) -> None:
        assert _sorted(["Vol 010", "Vol 9", "Vol 08"]) == ["Vol 08", "Vol 9", "Vol 010"]

    def test_letters_sort_case_insensitively(self) -> None:
        assert _sorted(["vol c", "Vol B", "VOL A"]) == ["VOL A", "Vol B", "vol c"]

    def test_full_width_digits_are_numbers(self) -> None:
        assert _sorted(["第１０巻", "第２巻", "第1巻"]) == ["第1巻", "第２巻", "第１０巻"]

    def test_several_numbers_in_one_name(self) -> None:
        assert _sorted(["S2 - 03", "S10 - 01", "S2 - 12"]) == ["S2 - 03", "S2 - 12", "S10 - 01"]

    def test_number_sorts_before_text_and_prefix_before_longer(self) -> None:
        assert _sorted(["Vol A", "Vol 1", "Vol 1.5", "Vol"]) == ["Vol", "Vol 1", "Vol 1.5", "Vol A"]

    def test_a_number_never_meets_a_string(self) -> None:
        # Mixed shapes must stay comparable (no int-vs-str TypeError).
        assert _sorted(["12", "abc", "12abc", "abc12", ""]) == ["", "12", "12abc", "abc", "abc12"]

    def test_kanji_numerals_after_dai_are_numbers(self) -> None:
        shuffled = ["第十巻", "第三巻", "第二十巻", "第一巻", "第十一巻", "第二巻", "第百二巻"]
        assert _sorted(shuffled) == [
            "第一巻",
            "第二巻",
            "第三巻",
            "第十巻",
            "第十一巻",
            "第二十巻",
            "第百二巻",
        ]

    @pytest.mark.parametrize("counter", ["巻", "話", "章", "集"])
    def test_kanji_numerals_before_a_counter_are_numbers(self, counter: str) -> None:
        names = [f"十{counter}", f"二{counter}", f"千{counter}", f"十一{counter}", f"一{counter}"]
        assert _sorted(names) == [
            f"一{counter}",
            f"二{counter}",
            f"十{counter}",
            f"十一{counter}",
            f"千{counter}",
        ]

    def test_a_kanji_numeral_standing_alone_is_a_number(self) -> None:
        assert _sorted(["十", "二", "十一", "一"]) == ["一", "二", "十", "十一"]
        # Alone between separators too: the usual "Title 三" volume name.
        assert _sorted(["鬼滅 十一", "鬼滅 三", "鬼滅 二十"]) == [
            "鬼滅 三",
            "鬼滅 十一",
            "鬼滅 二十",
        ]
        assert natural_key("鬼滅 (十)") == ((1, 0, "鬼滅 ("), (0, 10, ""), (1, 0, ")"))

    def test_positional_kanji_digits(self) -> None:
        # 一〇 = 10, 二五 = 25: digits written out one by one.
        assert _sorted(["第二五巻", "第一〇巻", "第九巻", "第一〇〇巻"]) == [
            "第九巻",
            "第一〇巻",
            "第二五巻",
            "第一〇〇巻",
        ]

    def test_kanji_and_arabic_numbers_share_one_scale(self) -> None:
        assert _sorted(["第十巻", "第9巻", "第二巻", "第１１巻", "第1巻"]) == [
            "第1巻",
            "第二巻",
            "第9巻",
            "第十巻",
            "第１１巻",
        ]
        assert natural_key("第二巻") == natural_key("第2巻")

    @pytest.mark.parametrize(
        "word",
        ["一番くじ", "十字架", "三国志", "五等分の花嫁", "二十世紀少年", "百人一首", "千夜一夜"],
    )
    def test_words_that_contain_a_numeral_stay_text(self, word: str) -> None:
        assert natural_key(word) == ((1, 0, word),)

    def test_numeral_words_keep_their_place_among_kanji_titles(self) -> None:
        # Read as 10, 十字架 would jump ahead of 丁寧 (a number sorts before
        # text) for no reason a reader could see.
        assert _sorted(["十字架", "一番くじ", "丁寧"]) == ["一番くじ", "丁寧", "十字架"]

    def test_a_malformed_numeral_stays_text(self) -> None:
        assert natural_key("第十十巻") == ((1, 0, "第十十巻"),)
        assert natural_key("第二三十巻") == ((1, 0, "第二三十巻"),)

    def test_volume_numbers_inside_a_titled_series(self) -> None:
        names = ["五等分の花嫁 第十巻", "五等分の花嫁 第二巻", "五等分の花嫁 第一巻"]
        assert _sorted(names) == [
            "五等分の花嫁 第一巻",
            "五等分の花嫁 第二巻",
            "五等分の花嫁 第十巻",
        ]

    def test_a_decimal_volume_is_one_number(self) -> None:
        # "." used to be compared with "巻": 第2.5巻 sorted before 第2巻.
        assert _sorted(["第3巻", "第2.5巻", "第2巻"]) == ["第2巻", "第2.5巻", "第3巻"]
        assert _sorted(["第３巻", "第２．５巻", "第２巻"]) == ["第２巻", "第２．５巻", "第３巻"]

    def test_decimals_compare_by_value_in_every_position(self) -> None:
        assert _sorted(["2.5 extra", "2 extra", "10 extra", "3 extra"]) == [
            "2 extra",
            "2.5 extra",
            "3 extra",
            "10 extra",
        ]
        assert _sorted(["Ch 10.5", "Ch 10.25", "Ch 10", "Ch 9.5"]) == [
            "Ch 9.5",
            "Ch 10",
            "Ch 10.25",
            "Ch 10.5",
        ]
        # Trailing zeros carry no value; a lone "." is punctuation, not a decimal.
        assert natural_key("Vol 2.50") == natural_key("Vol 2.5")
        assert natural_key("Vol 2.0") == natural_key("Vol 2")
        assert _sorted(["Vol.10", "Vol.2", "Vol.2.5"]) == ["Vol.2", "Vol.2.5", "Vol.10"]
        assert natural_key("2. Title") == ((0, 2, ""), (1, 0, ". title"))

    def test_the_japanese_library_example(self) -> None:
        names = [
            "第一巻", "第二巻", "第三巻", "第十巻", "第十一巻", "第2巻", "第2.5巻", "第3巻",
            "Volume 2", "Volume 10", "一番くじ", "十字架",
        ]  # fmt: skip
        # Equal keys (第2巻 / 第二巻) fall back to the raw name, as order_jobs does.
        assert sorted(names, key=lambda name: (natural_key(name), name)) == [
            "Volume 2", "Volume 10", "一番くじ", "十字架",
            "第一巻", "第2巻", "第二巻", "第2.5巻", "第3巻", "第三巻", "第十巻", "第十一巻",
        ]  # fmt: skip


# The third element of a job key is a GENERATION id, not an engine: two rows
# may run the same engine and each takes its own turn in the round-robin.
# `RANK` is what the worker builds from the enabled rows' LIST ORDER -- there
# is no cost model left to rank them by.
FIRST, SECOND = "g-1", "g-2"
RANK = {FIRST: 0, SECOND: 1}


def _jobs(generation: str, **series: list[str]) -> list[tuple[str, str, str]]:
    return [(name, volume, generation) for name, volumes in series.items() for volume in volumes]


class TestOrderJobs:
    def test_round_robin_across_series_with_uneven_counts(self) -> None:
        jobs = _jobs(
            FIRST,
            Alpha=["Volume 10", "Volume 2", "Volume 1"],
            Beta=["Volume 1"],
            Gamma=["Volume 2", "Volume 1"],
        )
        assert [(s, v) for s, v, _ in order_jobs(jobs, RANK)] == [
            ("Alpha", "Volume 1"),
            ("Beta", "Volume 1"),
            ("Gamma", "Volume 1"),
            ("Alpha", "Volume 2"),
            ("Gamma", "Volume 2"),
            ("Alpha", "Volume 10"),
        ]

    def test_input_order_is_irrelevant(self) -> None:
        jobs = _jobs(FIRST, B=["2", "1"], A=["1", "3"]) + _jobs(SECOND, B=["1"], A=["1"])
        assert order_jobs(jobs, RANK) == order_jobs(list(reversed(jobs)), RANK)

    def test_the_first_row_runs_entirely_before_the_second(self) -> None:
        # Row order IS the priority: the whole library gets the first row's
        # OCR layer before the second row starts.
        jobs = _jobs(SECOND, A=["1", "2"], B=["1"]) + _jobs(FIRST, A=["1", "2"], B=["1"])
        ordered = order_jobs(jobs, RANK)
        assert [g for _, _, g in ordered] == [FIRST] * 3 + [SECOND] * 3
        assert [(s, v) for s, v, _ in ordered[:3]] == [("A", "1"), ("B", "1"), ("A", "2")]
        assert [(s, v) for s, v, _ in ordered[3:]] == [("A", "1"), ("B", "1"), ("A", "2")]

    def test_position_counts_pending_volumes_only(self) -> None:
        # Only volume 7 of Alpha is left: it is Alpha's FIRST pending volume,
        # so it runs in round one, not in a seventh round after everything.
        jobs = _jobs(FIRST, Alpha=["Volume 7"], Beta=["Volume 1", "Volume 2"])
        assert [(s, v) for s, v, _ in order_jobs(jobs, RANK)] == [
            ("Alpha", "Volume 7"),
            ("Beta", "Volume 1"),
            ("Beta", "Volume 2"),
        ]

    def test_positions_are_per_generation(self) -> None:
        # Alpha's volume 1 already has the first row's sidecar; that must not
        # push its second-row job out of the second row's first round.
        jobs = _jobs(FIRST, Alpha=["2"], Beta=["1"]) + _jobs(SECOND, Alpha=["1", "2"], Beta=["1"])
        assert order_jobs(jobs, RANK) == [
            ("Alpha", "2", FIRST),
            ("Beta", "1", FIRST),
            ("Alpha", "1", SECOND),
            ("Beta", "1", SECOND),
            ("Alpha", "2", SECOND),
        ]

    def test_series_are_visited_in_natural_order(self) -> None:
        jobs = _jobs(FIRST, **{"Series 10": ["1"], "series 2": ["1"], "Series 1": ["1"]})
        assert [s for s, _, _ in order_jobs(jobs, RANK)] == ["Series 1", "series 2", "Series 10"]

    def test_round_resumes_after_the_series_served_last(self) -> None:
        # The worker recomputes the queue after every job. Without the cursor
        # Alpha (first by name) would be served again and again; with it the
        # turn passes on and the ORIGINAL projection is what actually runs.
        pending = _jobs(FIRST, Alpha=["1", "2", "3"], Beta=["1"], Gamma=["1", "2"])
        projection = order_jobs(pending, RANK)
        ran: list[tuple[str, str, str]] = []
        last_served: dict[str, str] = {}
        while pending:
            job = order_jobs(pending, RANK, last_served=last_served)[0]
            ran.append(job)
            pending.remove(job)
            last_served[job[2]] = job[0]
        assert ran == projection

    def test_cursor_of_a_vanished_series_still_rotates(self) -> None:
        jobs = _jobs(FIRST, Alpha=["1"], Delta=["1"])
        ordered = order_jobs(jobs, RANK, last_served={FIRST: "Beta"})
        assert [s for s, _, _ in ordered] == ["Delta", "Alpha"]

    def test_cursor_is_per_generation(self) -> None:
        jobs = _jobs(FIRST, A=["1"], B=["1"]) + _jobs(SECOND, A=["1"], B=["1"])
        ordered = order_jobs(jobs, RANK, last_served={SECOND: "A"})
        assert ordered == [
            ("A", "1", FIRST),
            ("B", "1", FIRST),
            ("B", "1", SECOND),
            ("A", "1", SECOND),
        ]

    def test_unranked_generation_runs_last(self) -> None:
        # A row that is not in the rank (disabled, or removed while its jobs
        # were already listed) goes after every ranked one.
        jobs = [("A", "1", "g-mystery"), ("A", "1", SECOND)]
        assert [g for _, _, g in order_jobs(jobs, RANK)] == [SECOND, "g-mystery"]

    def test_key_function_orders_arbitrary_records(self) -> None:
        records = [{"id": 1, "s": "B", "v": "1"}, {"id": 2, "s": "A", "v": "1"}]
        ordered = order_jobs(records, RANK, key=lambda r: (r["s"], r["v"], FIRST))
        assert [r["id"] for r in ordered] == [2, 1]
