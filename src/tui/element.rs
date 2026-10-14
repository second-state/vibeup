//! React 式渲染核心:`Element` 是纯描述(虚拟 DOM),组件函数
//! `(state) -> Element` 拼出整棵树,`render` 这一个通用渲染器负责把它画到
//! 终端。组件不碰 Frame,渲染器不认识业务。

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row as TRow, Table};
use ratatui::Frame;

/// UI 树的节点。全部是数据:组件产出它,`render` 消费它。
pub(super) enum Element {
    /// 不画任何东西。
    Nothing,
    /// 一段带样式的静态文本行。
    Lines {
        lines: Vec<Line<'static>>,
    },
    /// 垂直堆叠。`expand` 指定的子元素占满剩余空间(flex: 1),其余按内容
    /// 高度;`Overlay` 子元素不参与布局,最后叠画在上面。
    Column {
        expand: Option<usize>,
        children: Vec<Element>,
    },
    /// 分节框:只有一条上边框,节名嵌在框线上(像 HTML 的
    /// `<fieldset><legend>`),内部是子元素。
    Fieldset {
        title: String,
        children: Vec<Element>,
    },
    /// 表格。
    Table {
        header: Vec<&'static str>,
        rows: Vec<Vec<String>>,
        widths: Vec<Constraint>,
        selected: usize,
    },
    /// 居中模态框:按内容自适应大小的四边边框弹窗,`alert` 用红框红标题。
    Modal {
        title: String,
        alert: bool,
        body: Vec<Line<'static>>,
    },
    /// 浮层:不占布局空间,等兄弟元素画完后叠画。
    Overlay(Box<Element>),
}

impl Element {
    pub(super) fn lines(lines: Vec<Line<'static>>) -> Self {
        Self::Lines { lines }
    }

    pub(super) fn column(children: Vec<Element>) -> Self {
        Self::Column {
            expand: None,
            children,
        }
    }

    pub(super) fn column_expanding(expand: usize, children: Vec<Element>) -> Self {
        Self::Column {
            expand: Some(expand),
            children,
        }
    }

    /// 内容高度(行数)。布局用它给非 expand 子元素分空间。
    fn height(&self) -> u16 {
        match self {
            Self::Nothing | Self::Overlay(_) => 0,
            Self::Lines { lines } => lines.len() as u16,
            Self::Column { children, .. } => children.iter().map(Self::height).sum(),
            Self::Fieldset { children, .. } => children.iter().map(Self::height).sum::<u16>() + 1,
            Self::Table { rows, .. } => rows.len() as u16 + 1,
            // 模态框浮在父区域中央,布局高度按 0 算(它自己量内容)。
            Self::Modal { .. } => 0,
        }
    }

    pub(super) fn render(&self, f: &mut Frame, area: Rect) {
        match self {
            Self::Nothing => {}
            Self::Lines { lines } => {
                f.render_widget(Paragraph::new(lines.clone()), area);
            }
            Self::Column {
                expand,
                children,
            } => {
                let constraints: Vec<Constraint> = children
                    .iter()
                    .enumerate()
                    .map(|(i, c)| match expand {
                        Some(e) if *e == i => Constraint::Min(0),
                        _ => Constraint::Length(c.height()),
                    })
                    .collect();
                let areas = Layout::vertical(constraints).split(area);
                // 先画占布局的,再画浮层(Overlay),保证浮层在最上面。
                for (child, child_area) in children.iter().zip(areas.iter()) {
                    if !matches!(child, Self::Overlay(_)) {
                        child.render(f, *child_area);
                    }
                }
                for (child, _) in children.iter().zip(areas.iter()) {
                    if let Self::Overlay(inner) = child {
                        inner.render(f, area);
                    }
                }
            }
            Self::Fieldset { title, children } => {
                // 每节:只有一条上边框,节名嵌在框线上(像 <fieldset><legend>)。
                let block = Block::default()
                    .borders(Borders::TOP)
                    .title(format!(" {title} "))
                    .title_style(Style::default().add_modifier(Modifier::BOLD));
                let inner = block.inner(area);
                f.render_widget(block, area);
                let constraints: Vec<Constraint> = children
                    .iter()
                    .map(|c| Constraint::Length(c.height()))
                    .collect();
                let areas = Layout::vertical(constraints).split(inner);
                for (child, child_area) in children.iter().zip(areas.iter()) {
                    child.render(f, *child_area);
                }
            }
            Self::Table {
                header,
                rows,
                widths,
                selected,
            } => {
                let header = TRow::new(header.clone())
                    .style(Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));
                let rows: Vec<TRow> = rows
                    .iter()
                    .enumerate()
                    .map(|(i, cells)| {
                        let style = if i == *selected {
                            Style::default().bg(Color::DarkGray)
                        } else {
                            Style::default()
                        };
                        TRow::new(cells.iter().map(|c| Cell::from(c.clone()))).style(style)
                    })
                    .collect();
                let table = Table::new(rows, widths.clone()).header(header);
                f.render_widget(table, area);
            }
            Self::Modal { title, alert, body } => {
                // 居中弹窗,四周边框;告警态(失败/断连)用红框。
                let h = (body.len() as u16 + 2).min(area.height); // +2 = 上下边框
                let content_w = body.iter().map(Line::width).max().unwrap_or(0) as u16;
                let w = (content_w + 2 + 2) // +2 = 内容左右各留一空格,+2 = 边框
                    .clamp(22, area.width);
                let rect = Rect {
                    x: area.x + area.width.saturating_sub(w) / 2,
                    y: area.y + area.height.saturating_sub(h) / 2,
                    width: w,
                    height: h,
                };
                f.render_widget(Clear, rect);
                let err = Style::default().fg(Color::Red);
                let border_style = if *alert { err } else { Style::default() };
                let block = Block::bordered()
                    .title(Span::styled(
                        title.clone(),
                        border_style.add_modifier(Modifier::BOLD),
                    ))
                    .border_style(border_style);
                let inner = block.inner(rect);
                f.render_widget(block, rect);
                f.render_widget(Paragraph::new(body.clone()), inner);
            }
            Self::Overlay(inner) => inner.render(f, area),
        }
    }
}
