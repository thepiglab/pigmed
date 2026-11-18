//! Source adapters: one file per source (or per shared site technology), named after the registry `id`.
//!
//! An adapter does two things:
//! 1. `discover`: enumerate the source's documents as [`PlanRow`]s. Use a deterministic order and a finite list; obey
//!    `limit`; include version/date hints when the catalog shows them. Discovery is cheap, so this is where catalog
//!    pagination, facets and archives are reconciled.
//! 2. `extract`: fetch one document (all its renditions) and return a [`GuidelineDoc`]. Use the generic helpers in
//!    `super::html` (`blocks_from_html`, `blocks_from_markdown`) and `crate::convert::convert` for PDF/DOCX, then do
//!    the source-specific work: find the real content root, recommendation boxes and their grade/strength labels,
//!    version/status/dates, rights/licence text, related assets.
//!
//! Rules (see docs/GUIDELINES.md):
//! - Only use the [`Fetcher`]: it paces, obeys robots.txt, caches and detects login walls.
//!   An authorized API adapter may load its named runtime key; never include it in plans or diagnostics.
//! - Missing metadata stays `None`/"unknown". Never infer it from the domain, a neighbouring row or the fetch date.
//! - Keep every heading, block order and table. A section you cannot parse becomes a warning, not silence.
//! - Every adapter ships an offline fixture test (saved HTML/PDF under tests/fixtures/guidelines/<id>/) that pins
//!   block counts, headings, recommendation labels and metadata.
//!
//! Copy `_template.rs.txt` to `<id>.rs`, add it to `adapter_for` below, and set `"adapter": "<id>"` in the registry.
pub mod easl_public_pmc;
pub mod magicapp_public;
pub mod easl_pmc_cloud;
pub mod nice_html;
pub mod publisher_article;
pub mod pmc_crosswalk;
pub mod r2a_crosswalk;
pub mod r2b_crosswalk;
pub mod aan;
pub mod acep_clinical_policies;
pub mod aasm_sleep;
pub mod endocrine_society;
pub mod aap;
pub mod asco;
pub mod ers;
pub mod acog;
pub mod au_eviq;
pub mod guideline_central;
pub mod no_helsedirektoratet;
pub mod static_list;
pub mod common;
pub mod my_mahtas_cpg;
pub mod ng_fmoh_stg;
pub mod se_socialstyrelsen;
pub mod ats;
pub mod ash_hematology;
pub mod astro;
pub mod jp_minds;
pub mod gin_library;
pub mod cl_minsal_ges;
pub mod au_department_pregnancy_care;
pub mod au_cancer_council;
pub mod au_magic;
pub mod esmo;
pub mod ada;
pub mod acp_im;
pub mod acg;
pub mod esc;
pub mod eular_recommendations;
pub mod eso;
pub mod ata_thyroid;
pub mod bsg;
pub mod chest_guidelines;
pub mod rcog;
pub mod wses_guidelines;
pub mod aad_dermatology;
pub mod aao_hnsf;
pub mod aao_ophthalmology;
pub mod id_kemkes_pnpk;
pub mod ca_inesss;
pub mod has;
pub mod es_guiasalud;
pub mod eau;
pub mod au_living_evidence;
pub mod right_decisions;
pub mod aasld;
pub mod acr_radiology;
pub mod acr;
pub mod bts;
pub mod bashh;
pub mod kr_komgi;
pub mod kr_komgi_hwp;
pub mod registries_pdf;
pub mod et_efda_stg;
pub mod ke_moh_mental;
pub mod kdigo;
pub mod gold;
pub mod gina;
pub mod ctfphc;
pub mod za_stg_eml;
pub mod tz_moh_stg_nemlit;
pub mod india_mohfw_stg;
pub mod india_icmr_stw;
pub mod brain_trauma_foundation;
pub mod paho_iris;
pub mod who_publications_iris;
pub mod ug_moh_ucg;
pub mod rw_moh_stg;
pub mod asam_addiction;
pub mod aaos;
pub mod eu_jrc_cancer_guidelines;
pub mod sccm;
pub mod fi_current_care;
pub mod msf_medical;
pub mod va_dod;
pub mod uspstf;
pub mod au_rch;
pub mod sg_ace;
pub mod nih_hiv;
pub mod wgo_global_guidelines;
pub mod bc_guidelines;
pub mod phac_sti;
pub mod cdc_sti;
pub mod canadian_stroke;
pub mod rnao;
pub mod cdc_infection_control;
pub mod cenetec_section_fixture;
pub mod nz_starship;
pub mod sages_surgical_guidelines;
pub mod east_practice_guidelines;
pub mod ca_hypertension_canada;
pub mod au_racgp;
pub mod nci_pdq;
pub mod sign;
pub mod cdc_acip;
pub mod phac_naci;
pub mod mx_cenetec_cmgpc;
pub mod nl_fms_richtlijnendatabase;
pub mod au_sti;
pub mod idsa;
pub mod diabetes_canada;
pub mod nz_bpac;
pub mod brazil_conitec_pcdt;
pub mod iwgdf_diabetic_foot;
pub mod h2_catalog;

pub mod ca_cps;

pub mod ncbi_bookshelf;

use super::doc::GuidelineDoc;
use super::fetch::Fetcher;
use super::registry::SourceEntry;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One document to ingest (a plan file is JSONL of these).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanRow {
    pub source_id: String,
    /// Stable per work+version within the source; becomes the `doc_key`.
    pub doc_key: String,
    pub url: String,
    #[serde(default)]
    pub title: Option<String>,
    /// "html" | "pdf" | "docx" | "api" | ... (a hint; the fetched bytes decide)
    #[serde(default)]
    pub rendition: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub extra: Value,
}

#[async_trait::async_trait]
pub trait Adapter: Send + Sync {
    async fn discover(&self, src: &SourceEntry, f: &Fetcher, limit: Option<usize>) -> Result<Vec<PlanRow>>;
    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc>;
}

pub fn adapter_for(name: &str) -> Option<Box<dyn Adapter>> {
    match name {
        "easl_public_pmc" => Some(Box::new(easl_public_pmc::EaslPublicPmc)),
        "magicapp_public" => Some(Box::new(magicapp_public::MagicAppPublic::default())),
        "easl_pmc_cloud" => Some(Box::new(easl_pmc_cloud::EaslPmcCloud)),
        "ncbi_bookshelf" => Some(Box::new(ncbi_bookshelf::NcbiBookshelf)),
        "ca_cps" => Some(Box::new(ca_cps::CaCps)),
        "aan" => Some(Box::new(aan::Aan)),
        "acep_clinical_policies" => Some(Box::new(acep_clinical_policies::AcepClinicalPolicies)),
        "aasm_sleep" => Some(Box::new(aasm_sleep::AasmSleep)),
        "endocrine_society" => Some(Box::new(endocrine_society::EndocrineSociety)),
        "aap" => Some(Box::new(aap::Aap)),
        "asco" => Some(Box::new(asco::Asco)),
        "ers" => Some(Box::new(ers::Ers)),
        "acog" => Some(Box::new(acog::Acog)),
        "au_eviq" => Some(Box::new(au_eviq::AuEviq)),
        "guideline_central" => Some(Box::new(guideline_central::GuidelineCentral::default())),
        "no_helsedirektoratet" => Some(Box::new(no_helsedirektoratet::NoHelsedirektoratet::default())),
        "nice_html" => Some(Box::new(nice_html::NiceHtml)),
        "static_list" => Some(Box::new(static_list::StaticList)),
        "h2_static_catalog" => Some(Box::new(h2_catalog::H2StaticCatalog)),
        "va_dod" => Some(Box::new(va_dod::VaDod)),
        "uspstf" => Some(Box::new(uspstf::Uspstf)),
        "au_rch" => Some(Box::new(au_rch::AuRch)),
        "sg_ace" => Some(Box::new(sg_ace::SgAce)),
        "nih_hiv" => Some(Box::new(nih_hiv::NihHiv)),
        "wgo_global_guidelines" => Some(Box::new(wgo_global_guidelines::WgoGlobalGuidelines)),
        "bc_guidelines" => Some(Box::new(bc_guidelines::BcGuidelines)),
        "phac_sti" => Some(Box::new(phac_sti::PhacSti)),
        "cdc_sti" => Some(Box::new(cdc_sti::CdcSti)),
        "canadian_stroke" => Some(Box::new(canadian_stroke::CanadianStroke)),
        "rnao" => Some(Box::new(rnao::Rnao)),
        "cdc_infection_control" => Some(Box::new(cdc_infection_control::CdcInfectionControl)),
        "cenetec_section_fixture" => Some(Box::new(cenetec_section_fixture::CenetecSectionFixture)),
        "iwgdf_diabetic_foot" => Some(Box::new(iwgdf_diabetic_foot::Iwgdf)),
        "brazil_conitec_pcdt" => Some(Box::new(brazil_conitec_pcdt::BrazilConitecPcdt)),
        "nz_bpac" => Some(Box::new(nz_bpac::NzBpac)),
        "diabetes_canada" => Some(Box::new(diabetes_canada::DiabetesCanada)),
        "idsa" => Some(Box::new(idsa::Idsa)),
        "au_sti" => Some(Box::new(au_sti::AuSti)),
        "nl_fms_richtlijnendatabase" => Some(Box::new(nl_fms_richtlijnendatabase::NlFmsRichtlijnendatabase)),
        "mx_cenetec_cmgpc" => Some(Box::new(mx_cenetec_cmgpc::MxCenetecCmgpc)),
        "phac_naci" => Some(Box::new(phac_naci::PhacNaci)),
        "cdc_acip" => Some(Box::new(cdc_acip::CdcAcip)),
        "sign" => Some(Box::new(sign::Sign)),
        "nci_pdq" => Some(Box::new(nci_pdq::NciPdq)),
        "au_racgp" => Some(Box::new(au_racgp::AuRacgp)),
        "ca_hypertension_canada" => Some(Box::new(ca_hypertension_canada::HypertensionCanada)),
        "east_practice_guidelines" => Some(Box::new(east_practice_guidelines::East)),
        "sages_surgical_guidelines" => Some(Box::new(sages_surgical_guidelines::Sages)),
        "nz_starship" => Some(Box::new(nz_starship::NzStarship)),
        "msf_medical" => Some(Box::new(msf_medical::MsfMedical)),
        "fi_current_care" => Some(Box::new(fi_current_care::FiCurrentCare)),
        "sccm" => Some(Box::new(sccm::Sccm)),
        "eu_jrc_cancer_guidelines" => Some(Box::new(eu_jrc_cancer_guidelines::EuJrcCancerGuidelines)),
        "aaos" => Some(Box::new(aaos::Aaos)),
        "asam_addiction" => Some(Box::new(asam_addiction::Asam)),
        "rw_moh_stg" => Some(Box::new(rw_moh_stg::Rwanda)),
        "ug_moh_ucg" => Some(Box::new(ug_moh_ucg::Uganda)),
        "who_publications_iris" => Some(Box::new(who_publications_iris::WhoPublicationsIris)),
        "paho_iris" => Some(Box::new(paho_iris::PahoIris)),
        "brain_trauma_foundation" => Some(Box::new(brain_trauma_foundation::BrainTrauma)),
        "india_icmr_stw" => Some(Box::new(india_icmr_stw::IndiaIcmrStw)),
        "india_mohfw_stg" => Some(Box::new(india_mohfw_stg::IndiaMohfwStg)),
        "tz_moh_stg_nemlit" => Some(Box::new(tz_moh_stg_nemlit::TzMohStgNemlit)),
        "za_stg_eml" => Some(Box::new(za_stg_eml::ZaStgEml)),
        "ctfphc" => Some(Box::new(ctfphc::Ctfphc)),
        "gina" => Some(Box::new(gina::Gina)),
        "gold" => Some(Box::new(gold::Gold)),
        "kdigo" => Some(Box::new(kdigo::Kdigo)),
        "ke_moh_mental" => Some(Box::new(ke_moh_mental::KenyaMental)),
        "et_efda_stg" => Some(Box::new(et_efda_stg::EthiopiaStg)),
        "kr_komgi" => Some(Box::new(kr_komgi::KoreaKomgi)),
        "bashh" => Some(Box::new(bashh::Bashh)),
        "bts" => Some(Box::new(bts::Bts)),
        "acr" => Some(Box::new(acr::Acr)),
        "acr_radiology" => Some(Box::new(acr_radiology::AcrRadiology)),
        "aasld" => Some(Box::new(aasld::Aasld)),
        "right_decisions" => Some(Box::new(right_decisions::RightDecisions)),
        "au_living_evidence" => Some(Box::new(au_living_evidence::AuLivingEvidence)),
        "eau" => Some(Box::new(eau::Eau)),
        "es_guiasalud" => Some(Box::new(es_guiasalud::EsGuiasalud)),
        "has" => Some(Box::new(has::Has)),
        "ca_inesss" => Some(Box::new(ca_inesss::CaInesss)),
        "id_kemkes_pnpk" => Some(Box::new(id_kemkes_pnpk::IdKemkesPnpk)),
        "aao_ophthalmology" => Some(Box::new(aao_ophthalmology::AaoOphthalmology)),
        "aao_hnsf" => Some(Box::new(aao_hnsf::AaoHnsf)),
        "aad_dermatology" => Some(Box::new(aad_dermatology::AadDermatology)),
        "wses_guidelines" => Some(Box::new(wses_guidelines::WsesGuidelines)),
        "rcog" => Some(Box::new(rcog::Rcog)),
        "chest_guidelines" => Some(Box::new(chest_guidelines::Chest)),
        "bsg" => Some(Box::new(bsg::Bsg)),
        "ata_thyroid" => Some(Box::new(ata_thyroid::Ata)),
        "eso" => Some(Box::new(eso::Eso)),
        "eular_recommendations" => Some(Box::new(eular_recommendations::Eular)),
        "esc" => Some(Box::new(esc::Esc)),
        "acg" => Some(Box::new(acg::Acg)),
        "acp_im" => Some(Box::new(acp_im::AcpIm)),
        "ada" => Some(Box::new(ada::Ada)),
        "esmo" => Some(Box::new(esmo::Esmo)),
        "au_cancer_council" => Some(Box::new(au_cancer_council::AuCancerCouncil)),
        "au_department_pregnancy_care" => Some(Box::new(au_department_pregnancy_care::AuDepartmentPregnancyCare)),
        "cl_minsal_ges" => Some(Box::new(cl_minsal_ges::ChileGes)),
        "gin_library" => Some(Box::new(gin_library::GinLibrary)),
        "jp_minds" => Some(Box::new(jp_minds::JpMinds)),
        "astro" => Some(Box::new(astro::Astro)),
        "ash_hematology" => Some(Box::new(ash_hematology::AshHematology)),
        "ats" => Some(Box::new(ats::Ats)),
        "se_socialstyrelsen" => Some(Box::new(se_socialstyrelsen::SeSocialstyrelsen)),
        "ng_fmoh_stg" => Some(Box::new(ng_fmoh_stg::NgFmohStg)),
        "my_mahtas_cpg" => Some(Box::new(my_mahtas_cpg::MyMahtasCpg)),
        _ => None,
    }
}
