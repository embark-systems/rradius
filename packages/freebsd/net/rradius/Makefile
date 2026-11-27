PORTNAME=	rradius
DISTVERSIONPREFIX=	v
DISTVERSION=	0.1.0
CATEGORIES=	net security

MAINTAINER=	v@embark.systems
COMMENT=	RADIUS and TACACS+ server with Luau scripts and a built-in cluster database
WWW=		https://github.com/embark-systems/rradius

LICENSE=	BSD2CLAUSE DICTIONARIES
LICENSE_COMB=	multi
LICENSE_NAME_DICTIONARIES=	Creative Commons Attribution 4.0 (the RADIUS dictionaries)
LICENSE_FILE_BSD2CLAUSE=	${WRKSRC}/LICENSE
LICENSE_FILE_DICTIONARIES=	${WRKSRC}/share/LICENSE
LICENSE_PERMS_DICTIONARIES=	dist-mirror dist-sell pkg-mirror pkg-sell auto-accept

BUILD_DEPENDS=	cmake:devel/cmake-core

USES=		cargo
USE_GITHUB=	yes
GH_ACCOUNT=	embark-systems
USE_RC_SUBR=	rradius
USERS=		rradius
GROUPS=		rradius

CARGO_INSTALL_PATH=	rradius-server
# The default configuration path is fixed at build time: ${PREFIX}/etc/rradius/rradius.toml.
# The others are what the repository's .cargo/config.toml sets: LIBSQLITE3_FLAGS for the bundled
# SQLite, and the ac_cv_func_* pair so that jemalloc leaves the C library's thread cleanup and
# fork hooks alone (the server would crash, and refuses to start).
CARGO_ENV+=	RRADIUS_PREFIX=${PREFIX} \
		LIBSQLITE3_FLAGS=-DSQLITE_ENABLE_QPSG \
		ac_cv_func__malloc_thread_cleanup=no \
		ac_cv_func__pthread_mutex_init_calloc_cb=no
.include <bsd.port.options.mk>

.if ${ARCH} == aarch64
# jemalloc fixes the page size it supports at build time; 64 KiB runs on the smaller ones too.
CARGO_ENV+=	JEMALLOC_SYS_WITH_LG_PAGE=16
.endif

PLIST_SUB=	USER=${USERS}
SUB_LIST=	USER=${USERS} GROUP=${GROUPS}
PORTDOCS=	*

.include "${.CURDIR}/Makefile.crates"

OPTIONS_DEFINE=	DOCS

post-install:
	${MKDIR} ${STAGEDIR}${DATADIR} ${STAGEDIR}${ETCDIR} ${STAGEDIR}${PREFIX}/etc/newsyslog.conf.d
	cd ${WRKSRC}/share && ${COPYTREE_SHARE} . ${STAGEDIR}${DATADIR}
	${INSTALL_DATA} ${FILESDIR}/newsyslog.conf.sample ${STAGEDIR}${PREFIX}/etc/newsyslog.conf.d/rradius.conf.sample
	(cd ${STAGEDIR}${DATADIR} && ${FIND} -s . -type f | ${SED} -e 's|^\./|${DATADIR_REL}/|' >> ${TMPPLIST})

post-install-DOCS-on:
	${MKDIR} ${STAGEDIR}${DOCSDIR}
	cd ${WRKSRC}/docs && ${COPYTREE_SHARE} manual ${STAGEDIR}${DOCSDIR}
	${INSTALL_DATA} ${WRKSRC}/docs/usage.md ${STAGEDIR}${DOCSDIR}
	${INSTALL_DATA} ${FILESDIR}/README.packages ${STAGEDIR}${DOCSDIR}
	cd ${WRKSRC}/rradius-server && ${COPYTREE_SHARE} examples ${STAGEDIR}${DOCSDIR}
	(cd ${STAGEDIR}${DOCSDIR} && ${FIND} -s . -type f | ${SED} -e 's|^\./|%%PORTDOCS%%%%DOCSDIR%%/|' >> ${TMPPLIST})

.include <bsd.port.mk>
