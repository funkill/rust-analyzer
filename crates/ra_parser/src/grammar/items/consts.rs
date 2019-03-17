use super::*;

pub(super) fn static_def(p: &mut Parser) {
    const_or_static(p, STATIC_KW)
}

pub(super) fn const_def(p: &mut Parser) {
    const_or_static(p, CONST_KW)
}

fn const_or_static(p: &mut Parser, kw: SyntaxKind) {
    assert!(p.at(kw));
    p.bump();
    p.eat(MUT_KW); // FIXME: validator to forbid const mut
    name(p);
    types::ascription(p);
    if p.eat(EQ) {
        expressions::expr(p);
    }
    p.expect(SEMI);
}
