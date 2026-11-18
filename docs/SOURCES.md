# Primary sources checked November 11, 2026

These sources informed the design and adapter interfaces. They are not a claim that the released code has completed live integration with every service.

## Acquisition, formats, and rights

1. NLM, Download PubMed Data: https://pubmed.ncbi.nlm.nih.gov/download/
   Annual complete citation snapshot; new/revised/deleted updates; numerical update ordering; citation/abstract scope.
2. NLM, PMC Cloud Service: https://pmc.ncbi.nlm.nih.gov/tools/cloud/
   Distribution transition completed August 26, 2026; JSON, JATS XML, extracted text, PDFs/media/supplements where available; article-level licensing; no PDF/media/supplement distribution for non-CC author manuscripts; continuous version updates.
3. NLM, current PMC S3 README: https://pmc-oa-opendata.s3.amazonaws.com/README.txt
   Versioned keys; metadata schema; inventory manifest/fileSchema; ETags; terms for redistribution and snapshot labeling.
4. NLM, Accessing PMC Article Datasets Using AWS: https://pmc.ncbi.nlm.nih.gov/tools/pmcaws/
   Public bucket and anonymous/no-sign-request access.
5. NLM, developer access: https://pmc.ncbi.nlm.nih.gov/tools/developers/
   Authorized bulk/API channels; not systematic web-page scraping.
6. NLM, E-utilities rate guidance: https://support.nlm.nih.gov/knowledgebase/article/KA-05317/en-us
   E-utilities limits are distinct from the recommended cloud bulk acquisition route.
7. NCBI Bookshelf, FTP Service: https://www.ncbi.nlm.nih.gov/sites/books/about/ftp/
   A later educational-material source: OA book packages can include BITS XML, PDFs, and graphics. Separate source adapter/rights review required; not implemented in v0.1.

## Encoders and compression

8. H Company, NeoMME-260M-Retriever model card: https://huggingface.co/Hcompany/NeoMME-260M-Retriever
   Official processor/model interface; joint dense and multi-vector output; dimensions; 2048-pixel cap; text-to-document evaluation.
9. H Company, NeoMME technical report: https://arxiv.org/abs/2609.01657
   See PDF page 21, Table 7 / Figure 12: hierarchical pooling and asymmetric quantization; 6.0 kB/page at 95.19% retained relative nDCG, versus an FP32 late-vector baseline. Read as a benchmark-specific storage experiment, not guaranteed biomedical compression or inference speed.
10. Google, EmbeddingGemma 2 model card: https://huggingface.co/google/embeddinggemma-2
    Four modalities, common dense space, MRL dimensions, selective encoder loading, task prefixes, FP32/BF16 execution and FP16 warning.
11. Google, EmbeddingGemma 2 technical report, v2 November 9, 2026: https://arxiv.org/abs/2610.37364
    271M text / 439M text+vision / 744M all-modal configuration; vision-budget ablations; 32-frame evaluation setup; single-vector comparison scope.
12. Microsoft, BiomedCLIP model card: https://huggingface.co/microsoft/BiomedCLIP-PubMedBERT_256-vit_base_patch16_224
    Biomedical image/text specialist baseline; trained on PMC figure-caption data.

## Compute and publication

13. Site scheduler documentation (QOS, time limits, GPU access) for your cluster; recorded in the local site notes, not here.
14. Site documentation on node-local temporary directories (`$TMPDIR`); recorded in local site notes.
17. Hugging Face, Hub storage limits: https://huggingface.co/docs/hub/storage-limits
    Account-dependent quotas, repository/file/commit recommendations; public hosting is not an unlimited guaranteed dump.
18. Hugging Face, upload guide: https://huggingface.co/docs/huggingface_hub/guides/upload
    Repository commits and upload APIs. This package uses immutable bundles and explicit post-commit verification.

## Media prevalence and biomedical training

19. Gobeill and Ruch, Video Files in PubMed Central Supplementary Data: https://zenodo.org/records/20039507
    472,688 video files, snapshot March 5, 2026. File count is not the proportion of articles with video. Current unique-article prevalence must be measured with a stated denominator.
20. MIRIAD dataset: https://huggingface.co/datasets/miriad/miriad-4.4M
    Candidate synthetic biomedical question/evidence training pairs; review dataset and underlying-content terms. Not a substitute for independent clinical/scientific relevance labels.

## Which choices are proposals rather than published optima?

The 512-token/64-overlap text recipe, 20-second video windows, 30-second audio windows, eight initial burst workers, 200 GiB outbox, 256–512 MiB pilot shard targets, 96/2/2 group split, proposed training mixture, and storage scenarios are design assumptions to test. Neither encoder report establishes these as globally optimal PubMed settings.
