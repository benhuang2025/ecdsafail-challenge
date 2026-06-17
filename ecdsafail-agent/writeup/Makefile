# 编译中文 writeup（需要 xelatex + ctex/xeCJK）。
# 用法: make   （或: make clean）
# 注：ctex/xeCJK 安装在 user TEXMF 树，故导出 TEXMFHOME。
export TEXMFHOME := $(HOME)/Library/texmf

main.pdf: main.tex
	xelatex -interaction=nonstopmode -halt-on-error main.tex
	xelatex -interaction=nonstopmode -halt-on-error main.tex   # 第二遍生成目录/交叉引用

.PHONY: clean
clean:
	rm -f main.aux main.log main.out main.toc main.pdf
