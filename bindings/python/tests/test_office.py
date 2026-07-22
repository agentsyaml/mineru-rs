import asyncio
import io
import json
import os
import tempfile
import unittest
import zipfile
from pathlib import Path

import mineru_rs
from test_smoke import MockApi


def docx_bytes():
    types = b'''<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/></Types>'''
    relationships = b'''<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>'''
    document = b'''<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Installed wheel Office proof</w:t></w:r></w:p><w:sectPr/></w:body></w:document>'''
    document_rels = b'''<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>'''
    styles = b'''<?xml version="1.0"?><w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"/>'''
    output = io.BytesIO()
    with zipfile.ZipFile(output, "w", zipfile.ZIP_DEFLATED) as archive:
        for name, data in {
            "[Content_Types].xml": types,
            "_rels/.rels": relationships,
            "word/document.xml": document,
            "word/_rels/document.xml.rels": document_rels,
            "word/styles.xml": styles,
        }.items():
            archive.writestr(name, data)
    return output.getvalue()


class OfficeWheelTest(unittest.TestCase):
    @unittest.skipUnless(os.environ.get("MINERU_RUN_OFFICE_E2E") == "1", "real Office conversion e2e")
    def test_bundled_helper_converts_real_generated_docx(self):
        async def scenario(root):
            source = root / "sample.docx"
            docx = docx_bytes()
            source.write_bytes(docx)
            middle = json.dumps(
                {
                    "pdf_info": [
                        {
                            "page_idx": 0,
                            "page_size": [612, 792],
                            "preproc_blocks": [],
                            "discarded_blocks": [],
                        }
                    ]
                }
            ).encode()
            files = {
                "sample/office/sample_middle.json": middle,
                "sample/office/sample_origin.docx": docx,
            }
            output = root / "output"
            output.mkdir()
            with MockApi(files=files) as api:
                report = await mineru_rs.run(source, output, api_url=api.url)
            self.assertEqual(report.warnings, [])
            layout = output / "sample/office/sample_layout.pdf"
            self.assertTrue(layout.read_bytes().startswith(b"%PDF-"))

        with tempfile.TemporaryDirectory() as tmp:
            asyncio.run(scenario(Path(tmp)))


if __name__ == "__main__":
    unittest.main()
