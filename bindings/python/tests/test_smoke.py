import mineru_rs


def test_canonical_stem_sanitizes():
    assert mineru_rs.canonical_stem("a bad/pdf") == "a_bad_pdf"


def test_canonical_stem_empty_defaults_to_document():
    assert mineru_rs.canonical_stem("") == "document"


def test_canonical_stem_rejects_windows_device_name():
    try:
        mineru_rs.canonical_stem("con")
        raise AssertionError("expected ValueError for reserved device name")
    except ValueError:
        pass


def test_validate_pdf_options_accepts_defaults():
    assert mineru_rs.validate_pdf_options(0, None, True, True, True) is True


def test_validate_pdf_options_rejects_inverted_range():
    try:
        mineru_rs.validate_pdf_options(5, 2, True, True, True)
        raise AssertionError("expected ValueError for inverted range")
    except ValueError:
        pass
